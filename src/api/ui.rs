use std::sync::MutexGuard;
use std::time::{Duration, Instant};

use agent_first_ui::{UiExpiry, UiPagePolicy, UiPageScript, UiSecurityPolicy};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_TYPE, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::server::{ApiState, api_error, validate_session_id};
use crate::TerminalSessionManager;

mod frontend;
mod runtime;

pub use frontend::{PROVIDER_ID, TerminalUi, UI_KIND};

use std::sync::Arc;

const UI_ACCESS_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
const UI_APP_JS: &str = include_str!("ui/app.js");

fn ui_access_idle_ttl() -> Duration {
    Duration::from_millis(UI_ACCESS_IDLE_TTL_MS)
}

/// Opaque controller for the terminal page's AFUI session runtime.
pub struct TerminalUiRuntime(runtime::TerminalRuntime);

pub fn attach_runtime<T>(
    session: agent_first_ui::UiSession<T>,
) -> agent_first_ui::Result<(agent_first_ui::UiSession<T>, TerminalUiRuntime)>
where
    T: Send + 'static,
{
    let (session, runtime) = session.with_runtime::<
        runtime::TerminalUiAction,
        runtime::TerminalUiReply,
        runtime::TerminalUiState,
    >()?;
    Ok((session, TerminalUiRuntime(runtime)))
}

pub fn publish_opening_state(
    state: &ApiState,
    runtime: &TerminalUiRuntime,
) -> agent_first_ui::Result<()> {
    runtime::publish_opening_state(state, &runtime.0)
}

pub async fn run_runtime(state: ApiState, runtime: TerminalUiRuntime) {
    runtime::run(state, runtime.0).await;
}

/// The security posture both deliveries of this page share.
///
/// The page's own script and stylesheet, images from this same origin, inline
/// styles for the terminal geometry the stylesheet cannot express, and a
/// connection back to its own session. Nothing remote, no framing, and no form
/// target at all: everything a person does here travels over the session.
pub fn ui_security_policy() -> UiSecurityPolicy {
    UiPagePolicy::new(UiPageScript::SameOrigin)
        .allow_images()
        .allow_inline_styles()
        .allow_runtime()
        .into_security_policy()
}

/// The UI with no credential in its paths, for AFUI to serve.
///
/// AFUI owns the listener, the credential in the URL, the security headers, and
/// the window, so `afterminal ui` hands it these routes and nothing else.
pub fn ui_router(ui: Arc<TerminalUi>) -> Router {
    Router::new()
        .route("/app.js", get(app_js))
        .merge(frontend_routes(&ui))
}

/// The page, the stylesheet and the frontend's assets: everything a person can
/// replace, and nothing a person can.
///
/// No route under the reserved prefix, in either delivery: AFUI's session host
/// answers those for a session, and its mount dispatch answers them for the
/// credentials this API issues itself.
fn frontend_routes(ui: &Arc<TerminalUi>) -> Router {
    agent_first_ui::page_routes(
        ui.page().to_owned(),
        ui.stylesheet().to_vec(),
        ui.frontend(),
    )
}

pub(super) fn attachment_routes() -> Router<ApiState> {
    Router::new()
        .route("/ui-attachments", post(create_ui_attachment))
        .route(
            "/ui-attachments/{ui_token}",
            put(keep_alive_ui_attachment).delete(revoke_ui_attachment),
        )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUiAttachmentRequest {
    initial_session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateUiAttachmentResult {
    /// The whole URL, not the credential alone.
    ///
    /// The authority in it is the one this API answers UI requests under, and
    /// a request arriving under any other name is refused — so a caller that
    /// assembled its own URL from whatever it happened to dial would be
    /// guessing at something only this side knows.
    ui_access_url_secret: String,
    ui_access_idle_timeout_ms: u64,
    initial_session_id: Option<String>,
}

async fn create_ui_attachment(
    State(state): State<ApiState>,
    body: Result<Json<CreateUiAttachmentRequest>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let Json(request) = match body {
        Ok(request) => request,
        Err(error) => {
            return api_error(started, error.status(), "invalid_json", error.body_text());
        }
    };
    if let Some(session_id) = request.initial_session_id.as_deref() {
        if let Err(message) = validate_session_id(session_id) {
            return api_error(
                started,
                StatusCode::BAD_REQUEST,
                "invalid_session_id",
                message,
            );
        }
        let mut manager = match lock_manager(&state) {
            Ok(manager) => manager,
            Err(()) => return runtime_lock_error(),
        };
        let _status = manager.status(session_id);
        if manager.metadata(session_id).is_none() {
            return api_error(
                started,
                StatusCode::NOT_FOUND,
                "session_not_found",
                format!("terminal session `{session_id}` not found"),
            );
        }
    }
    let Some(access) = state.ui_access() else {
        return ui_mount_missing();
    };
    let (credential, runtime) = match access
        .issue_with_runtime::<
            runtime::TerminalUiAction,
            runtime::TerminalUiReply,
            runtime::TerminalUiState,
        >(UiExpiry::Idle(ui_access_idle_ttl()))
    {
        Ok(issued) => issued,
        Err(error) => {
            return api_error(
                started,
                StatusCode::INTERNAL_SERVER_ERROR,
                "ui_capability_generation_failed",
                error.to_string(),
            );
        }
    };
    if let Err(error) = runtime::publish_opening_state(&state, &runtime) {
        let _revoked = access.revoke(credential.secret());
        return api_error(
            started,
            StatusCode::INTERNAL_SERVER_ERROR,
            "ui_runtime_initialization_failed",
            error.to_string(),
        );
    }
    tokio::spawn(runtime::run(state, runtime));
    Json(CreateUiAttachmentResult {
        ui_access_url_secret: credential.access_url_secret().to_owned(),
        ui_access_idle_timeout_ms: UI_ACCESS_IDLE_TTL_MS,
        initial_session_id: request.initial_session_id,
    })
    .into_response()
}

async fn keep_alive_ui_attachment(
    State(state): State<ApiState>,
    Path(ui_token): Path<String>,
) -> Response {
    let Some(access) = state.ui_access() else {
        return ui_mount_missing();
    };
    if access.renew(&ui_token) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        ui_attachment_not_found()
    }
}

async fn revoke_ui_attachment(
    State(state): State<ApiState>,
    Path(ui_token): Path<String>,
) -> Response {
    let Some(access) = state.ui_access() else {
        return ui_mount_missing();
    };
    if access.revoke(&ui_token) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        ui_attachment_not_found()
    }
}

fn ui_attachment_not_found() -> Response {
    api_error(
        Instant::now(),
        StatusCode::NOT_FOUND,
        "ui_attachment_not_found",
        "terminal UI attachment not found",
    )
}

/// afterminal's own behaviour, and not a frontend's to replace: AFUI refuses a
/// frontend file whose name says it is a script, and the page that loads this
/// one loads it because afterminal spliced the tag in, not because a template
/// wrote it.
///
/// AFUI's page kernel comes first, in the same script rather than a second
/// request. Its session runtime owns every browser conversation; this file is
/// only terminal interaction and rendering.
async fn app_js() -> Response {
    (
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("text/javascript; charset=utf-8"),
        )],
        format!("{}\n{UI_APP_JS}", agent_first_ui::page_kernel_source()),
    )
        .into_response()
}

fn lock_manager(state: &ApiState) -> Result<MutexGuard<'_, TerminalSessionManager>, ()> {
    state.manager.lock().map_err(|_| ())
}

fn runtime_lock_error() -> Response {
    api_error(
        Instant::now(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "runtime_lock_poisoned",
        "terminal runtime state is unavailable",
    )
}

/// Reachable only if a caller built this state and served it without going
/// through `router`, which is the one place that installs the mount.
fn ui_mount_missing() -> Response {
    api_error(
        Instant::now(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "ui_mount_unavailable",
        "terminal API is not serving a UI mount",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::header::{AUTHORIZATION, CONTENT_SECURITY_POLICY, HOST, HeaderValue};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::super::server::{TEST_API_AUTHORITY, test_router};
    use super::{TerminalUi, UI_ACCESS_IDLE_TTL_MS, UI_APP_JS, ui_router};
    use crate::api::ApiState;

    const API_TOKEN: &str = "ui-test-api-token-0123456789-abcdefgh";

    fn builtin_ui() -> Arc<TerminalUi> {
        Arc::new(
            TerminalUi::resolve(std::path::Path::new("/nonexistent"))
                .expect("the built-in terminal page renders"),
        )
    }

    #[test]
    fn the_browser_has_one_afui_runtime_and_no_private_transport() {
        assert!(UI_APP_JS.contains("afui.connect({"));
        for forbidden in [
            "afui.request(",
            "afui.stream(",
            "afui.channel(",
            "EventSource",
            "WebSocket",
            "request_id",
            "sequenceKey",
            "/sessions",
        ] {
            assert!(!UI_APP_JS.contains(forbidden), "found {forbidden}");
        }
    }

    #[tokio::test]
    async fn hosted_page_and_assets_load_without_a_private_ui_api() {
        let app = ui_router(builtin_ui());
        let page = call_text(&app, "/").await;
        assert_eq!(page.0, StatusCode::OK);
        assert!(page.1.contains("Shared terminal sessions"));
        assert!(page.1.contains("app.js"));
        assert!(!page.1.contains(API_TOKEN));

        for asset in ["app.js", "style.css"] {
            let response = call_text(&app, &format!("/{asset}")).await;
            assert_eq!(response.0, StatusCode::OK, "{asset}");
            assert!(!response.1.contains(API_TOKEN), "{asset}");
        }

        assert_eq!(call_text(&app, "/sessions").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn running_api_issues_runtime_backed_revocable_attachments() {
        let app = test_router(ApiState::new(API_TOKEN.to_string()));
        let unauthenticated = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/ui-attachments")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("request"),
        )
        .await;
        assert_eq!(unauthenticated.0, StatusCode::UNAUTHORIZED);

        let first = issue_ui_attachment(&app).await;
        let second = issue_ui_attachment(&app).await;
        assert_ne!(first, second);

        let kept_alive = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/ui-attachments/{first}"))
                    .header(AUTHORIZATION, format!("Bearer {API_TOKEN}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(kept_alive.status(), StatusCode::NO_CONTENT);

        assert_eq!(
            call_text(&app, "/ui/not-the-token/").await.0,
            StatusCode::NOT_FOUND
        );
        for token in [&first, &second] {
            let page = ui_page(&app, token).await;
            assert_eq!(page.status(), StatusCode::OK);
            assert!(page.headers().contains_key(CONTENT_SECURITY_POLICY));
            assert_eq!(
                call_text(&app, &format!("/ui/{token}/app.js")).await.0,
                StatusCode::OK
            );
            assert_eq!(
                call_text(&app, &format!("/ui/{token}/sessions")).await.0,
                StatusCode::NOT_FOUND
            );
        }

        let revoked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/ui-attachments/{first}"))
                    .header(AUTHORIZATION, format!("Bearer {API_TOKEN}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
        assert_eq!(ui_page(&app, &first).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(ui_page(&app, &second).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_rebound_name_does_not_reach_the_terminal() {
        let app = test_router(ApiState::new(API_TOKEN.to_string()));
        let ui_token = issue_ui_attachment(&app).await;
        let rebound = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/{ui_token}/"))
                    .header(HOST, "terminal.attacker.test")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(rebound.status(), StatusCode::FORBIDDEN);
        assert_eq!(ui_page(&app, &ui_token).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn reserved_routes_answer_on_an_attached_credential() {
        let app = test_router(ApiState::new(API_TOKEN.to_string()));
        let ui_token = issue_ui_attachment(&app).await;

        let icon = call_text(
            &app,
            &format!("/ui/{ui_token}/{}", agent_first_ui::APP_ICON_PATH),
        )
        .await;
        assert_eq!(icon.0, StatusCode::OK);
        assert!(icon.1.contains("<svg"), "{}", icon.1);

        let ended = call(
            &app,
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/ui/{ui_token}/{}",
                    agent_first_ui::SESSION_END_PATH
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(ended.0, StatusCode::NO_CONTENT);
        assert_eq!(
            ui_page(&app, &ui_token).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    async fn call_text(app: &axum::Router, uri: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(HOST, TEST_API_AUTHORITY)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn issue_ui_attachment(app: &axum::Router) -> String {
        let response = call(
            app,
            Request::builder()
                .method("POST")
                .uri("/ui-attachments")
                .header(AUTHORIZATION, format!("Bearer {API_TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("request"),
        )
        .await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        assert_eq!(
            response.1["ui_access_idle_timeout_ms"],
            UI_ACCESS_IDLE_TTL_MS
        );
        let url = response.1["ui_access_url_secret"]
            .as_str()
            .expect("UI access URL");
        assert!(
            url.starts_with(&format!("http://{TEST_API_AUTHORITY}/ui/")),
            "{url}"
        );
        url.trim_end_matches('/')
            .rsplit('/')
            .next()
            .expect("the credential is the last segment")
            .to_string()
    }

    async fn ui_page(app: &axum::Router, ui_token: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/{ui_token}/"))
                    .header(HOST, TEST_API_AUTHORITY)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    async fn call(app: &axum::Router, mut request: Request<Body>) -> (StatusCode, Value) {
        request
            .headers_mut()
            .entry(HOST)
            .or_insert(HeaderValue::from_static(TEST_API_AUTHORITY));
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("JSON response")
        };
        (status, value)
    }
}
