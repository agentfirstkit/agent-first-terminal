use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use agent_first_ui::{UiMount, UiMountAccess};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE,
};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Serialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;

use super::model::{
    AcquireInputLeaseRequest, ActorKindName, ActorModel, EventEnvelopeResult, HealthResult,
    InputAck, InputLeaseListResult, InputLeaseResult, OpenSessionRequest, ResizeRequest,
    ScreenResult, SecretInputActionRequest, SecretInputResult, SendInputRequest, SendSignalRequest,
    SessionInfo, SessionListResult, SignalAck,
};
use super::schema::{openapi_document, schema_index, standalone_schemas};
use super::ui;
use crate::{
    EventEnvelope, InputActor, MAX_TERMINAL_DIMENSION, MIN_TERMINAL_DIMENSION, TerminalError,
    TerminalSessionManager,
};

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");

#[derive(Clone)]
pub struct ApiState {
    pub(super) manager: Arc<Mutex<TerminalSessionManager>>,
    access_token_secret: Arc<Vec<u8>>,
    /// Filled by [`router`] once the UI router this state serves exists.
    ///
    /// The three form a cycle — the UI router needs this state, AFUI's mount
    /// needs that router, and the attachment endpoints here need the mount —
    /// so one of them has to be installed after construction rather than
    /// before. This is that one, and `router` is the only place that fills it.
    ui_access: Arc<OnceLock<UiMountAccess>>,
}

struct ApiFailure {
    started: Instant,
    status: StatusCode,
    code: &'static str,
    message: String,
    retryable: bool,
    hint: Option<&'static str>,
}

impl ApiFailure {
    fn new(
        started: Instant,
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            started,
            status,
            code,
            message: message.into(),
            retryable: false,
            hint: None,
        }
    }

    /// Mark a failure the same call can survive later — a lock, a lease, or a
    /// secret-input window the person has not finished with yet.
    fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    fn hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(hint);
        self
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let mut builder = agent_first_data::json_error(self.code, &self.message)
            .retryable_if(self.retryable)
            .trace(trace(self.started));
        if let Some(hint) = self.hint {
            builder = builder.hint(hint);
        }
        let value: Value = match builder.build() {
            Ok(event) => event.into(),
            // The builder only rejects an empty code or message, both of which
            // are constants here; still, an error path may not panic.
            Err(_) => json!({
                "kind": "error",
                "error": {
                    "code": "api_error_envelope_failed",
                    "message": "the terminal API could not describe its own failure",
                    "retryable": false,
                },
                "trace": trace(self.started),
            }),
        };
        let mut response = json_response(self.status, value);
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"afterminal\""),
            );
        }
        response
    }
}

fn trace(started: Instant) -> Value {
    json!({"duration_ms": started.elapsed().as_millis() as u64})
}

/// The one serialization boundary every contract response goes through.
///
/// Baseline §6: a domain response is an AFDATA result envelope, and the raw
/// HTTP serialization calls AFDATA redaction — so a `_secret`-suffixed field
/// added anywhere upstream is blanked here instead of reaching a client.
fn result_response(started: Instant, payload: &impl Serialize) -> Response {
    let payload = match serde_json::to_value(payload) {
        Ok(payload) => payload,
        Err(error) => {
            return ApiFailure::new(
                started,
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_result_serialization_failed",
                error.to_string(),
            )
            .into_response();
        }
    };
    let event: Value = agent_first_data::json_result(payload)
        .trace(trace(started))
        .build()
        .into();
    json_response(StatusCode::OK, agent_first_data::redacted_value(&event))
}

/// A document that is its own contract — the OpenAPI paper and the standalone
/// Schemas — served under its own media type rather than in the envelope.
fn document_response(value: Value, media_type: &'static str) -> Response {
    let mut response = Json(agent_first_data::redacted_value(&value)).into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(media_type));
    response
}

fn json_response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

impl ApiState {
    pub fn new(access_token_secret: String) -> Self {
        Self::with_manager(TerminalSessionManager::new(), access_token_secret)
    }

    /// One state shared by the API and by whatever serves the UI. The UI's own
    /// capability is separate and short-lived, so browser code never receives
    /// the API bearer credential.
    pub fn with_manager(manager: TerminalSessionManager, access_token_secret: String) -> Self {
        Self {
            manager: Arc::new(Mutex::new(manager)),
            access_token_secret: Arc::new(access_token_secret.into_bytes()),
            ui_access: Arc::new(OnceLock::new()),
        }
    }

    /// The credentials this API issues against its own `/ui` mount, once
    /// [`router`] has built it.
    pub(super) fn ui_access(&self) -> Option<&UiMountAccess> {
        self.ui_access.get()
    }
}

/// The API, with its UI mounted under `/ui`.
///
/// `api_base_url` is where this API answers from the outside — the same URL it
/// advertises when it comes up, for example `http://192.168.1.9:9418`. AFUI
/// pins it: a UI request arriving under some other name is refused, so a DNS
/// name rebound onto this socket cannot reach the terminal. It is the caller's
/// to supply because only the caller knows which of its interfaces it told
/// anybody about; a bound `0.0.0.0` names nothing anyone can open.
pub fn router(
    state: ApiState,
    ui: std::sync::Arc<ui::TerminalUi>,
    api_base_url: &str,
) -> Result<Router, agent_first_ui::Error> {
    // The prefix lives here and nowhere else: `nest_service` below and the
    // URLs AFUI mints have to agree, and two literals cannot.
    let mount = UiMount::new(format!("{}/ui/", api_base_url.trim_end_matches('/')))?;
    let app_icon = ui.app_icon().clone();
    state
        .ui_access
        .set(mount.external_access(ui::ui_router(ui), ui::ui_security_policy(), Some(app_icon)))
        .map_err(|_| {
            agent_first_ui::Error::Io(std::io::Error::other(
                "terminal API state was already serving a UI mount",
            ))
        })?;
    let public = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/schemas/index.json", get(schemas_index))
        .route("/schemas/{schema_file}", get(schema))
        .method_not_allowed_fallback(method_not_allowed);
    let protected = Router::new()
        .route("/v1/sessions", get(list_sessions).post(open_session))
        .route(
            "/v1/sessions/{session_id}",
            get(get_session).delete(close_session),
        )
        .route("/v1/sessions/{session_id}/screen", get(get_screen))
        .route("/v1/sessions/{session_id}/input", post(send_input))
        .route("/v1/sessions/{session_id}/resize", post(resize_session))
        .route("/v1/sessions/{session_id}/signal", post(send_signal))
        .route(
            "/v1/sessions/{session_id}/secret-input",
            get(get_secret_input),
        )
        .route(
            "/v1/sessions/{session_id}/secret-input/actions",
            post(secret_input_action),
        )
        .route(
            "/v1/sessions/{session_id}/leases",
            get(list_input_leases).post(acquire_input_lease),
        )
        .route(
            "/v1/sessions/{session_id}/leases/{lease_id}",
            axum::routing::delete(release_input_lease),
        )
        .route("/v1/events", get(stream_events))
        .merge(ui::attachment_routes())
        .method_not_allowed_fallback(method_not_allowed)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));

    Ok(public
        .merge(protected)
        .fallback(not_found)
        .with_state(state)
        // AFUI's own dispatch, not a second copy of one: the credential check,
        // the 404 that does not confirm a UI is here, the `Host` and `Origin`
        // checks, the UI's response headers, the reserved `__afui/` prefix,
        // the Provider icon route and `__afui/end` all arrive with it.
        .nest_service("/ui", mount.router())
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(security_headers)))
}

async fn health() -> Response {
    result_response(
        Instant::now(),
        &HealthResult {
            service: "afterminal",
            version: env!("CARGO_PKG_VERSION"),
            status: "ready",
        },
    )
}

async fn openapi() -> Response {
    document_response(
        openapi_document(),
        "application/vnd.oai.openapi+json;version=3.2",
    )
}

async fn schemas_index() -> Response {
    document_response(schema_index(), "application/json")
}

async fn schema(Path(schema_file): Path<String>) -> Response {
    let started = Instant::now();
    match standalone_schemas().remove(&schema_file) {
        Some(schema) => document_response(schema, "application/schema+json"),
        None => ApiFailure::new(
            started,
            StatusCode::NOT_FOUND,
            "schema_not_found",
            "JSON Schema not found",
        )
        .hint("read /schemas/index.json for the schemas this process serves")
        .into_response(),
    }
}

async fn open_session(
    State(state): State<ApiState>,
    body: Result<Json<OpenSessionRequest>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let request = match json_body(started, body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    if let Err(message) = validate_open_request(&request) {
        return api_error(started, StatusCode::BAD_REQUEST, "invalid_request", message);
    }
    let session_id = request.session_id.clone();
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = manager.open(session_id.clone(), request.into_spec()) {
        return terminal_error(started, error);
    }
    match session_info(started, &mut manager, &session_id) {
        Ok(info) => result_response(started, &info),
        Err(error) => error.into_response(),
    }
}

async fn list_sessions(State(state): State<ApiState>) -> Response {
    let started = Instant::now();
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    let ids = manager.ids();
    let mut sessions = Vec::with_capacity(ids.len());
    for session_id in ids {
        if let Ok(info) = session_info(started, &mut manager, &session_id) {
            sessions.push(info);
        }
    }
    result_response(started, &SessionListResult { sessions })
}

async fn get_session(State(state): State<ApiState>, Path(session_id): Path<String>) -> Response {
    let started = Instant::now();
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    match session_info(started, &mut manager, &session_id) {
        Ok(info) => result_response(started, &info),
        Err(error) => error.into_response(),
    }
}

async fn get_screen(State(state): State<ApiState>, Path(session_id): Path<String>) -> Response {
    let started = Instant::now();
    let manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    match manager.screen(&session_id) {
        Some(screen) => result_response(started, &ScreenResult::from(screen)),
        None => api_error(
            started,
            StatusCode::NOT_FOUND,
            "session_not_found",
            format!("terminal session `{session_id}` not found"),
        ),
    }
}

async fn get_secret_input(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Response {
    let started = Instant::now();
    let manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    match manager.secret_input(&session_id) {
        Ok(status) => result_response(started, &SecretInputResult::new(session_id, status)),
        Err(error) => terminal_error(started, error),
    }
}

async fn secret_input_action(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    body: Result<Json<SecretInputActionRequest>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let request = match json_body(started, body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    if let Err(message) = validate_actor(request.actor()) {
        return api_error(started, StatusCode::BAD_REQUEST, "invalid_actor", message);
    }
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    let outcome = match request {
        SecretInputActionRequest::Start { actor, reason } => {
            manager.enter_secret(&session_id, actor.into(), &reason)
        }
        SecretInputActionRequest::End { actor } => manager.exit_secret(&session_id, actor.into()),
    };
    match outcome {
        Ok(status) => result_response(started, &SecretInputResult::new(session_id, status)),
        Err(error) => terminal_error(started, error),
    }
}

async fn send_input(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    body: Result<Json<SendInputRequest>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let request = match json_body(started, body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    if let Err(message) = validate_actor(&request.actor) {
        return api_error(started, StatusCode::BAD_REQUEST, "invalid_actor", message);
    }
    if let Some(lease_id) = request.lease_id.as_deref()
        && let Err(message) = validate_identifier("lease_id", lease_id)
    {
        return api_error(
            started,
            StatusCode::BAD_REQUEST,
            "invalid_lease_id",
            message,
        );
    }
    let bytes = match base64::engine::general_purpose::STANDARD.decode(request.data_base64) {
        Ok(bytes) => bytes,
        Err(error) => {
            return api_error(
                started,
                StatusCode::BAD_REQUEST,
                "invalid_input_base64",
                format!("data_base64 is invalid: {error}"),
            );
        }
    };
    if bytes.len() > MAX_INPUT_BYTES {
        return api_error(
            started,
            StatusCode::PAYLOAD_TOO_LARGE,
            "input_too_large",
            format!("decoded input exceeds {MAX_INPUT_BYTES} bytes"),
        );
    }
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    let actor = InputActor::from(request.actor);
    match manager.write_as(
        &session_id,
        actor.clone(),
        request.lease_id.as_deref(),
        &bytes,
    ) {
        Ok(()) => result_response(
            started,
            &InputAck {
                accepted: true,
                input_bytes: bytes.len(),
                actor: actor.into(),
            },
        ),
        Err(error) => terminal_error(started, error),
    }
}

async fn resize_session(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    body: Result<Json<ResizeRequest>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let request = match json_body(started, body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    if let Err(message) = validate_dimensions(request.rows, request.cols) {
        return api_error(
            started,
            StatusCode::BAD_REQUEST,
            "invalid_dimensions",
            message,
        );
    }
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = manager.resize(&session_id, request.rows, request.cols) {
        return terminal_error(started, error);
    }
    match session_info(started, &mut manager, &session_id) {
        Ok(info) => result_response(started, &info),
        Err(error) => error.into_response(),
    }
}

async fn send_signal(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    body: Result<Json<SendSignalRequest>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let request = match json_body(started, body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    if let Err(message) = validate_actor(&request.actor) {
        return api_error(started, StatusCode::BAD_REQUEST, "invalid_actor", message);
    }
    if let Some(lease_id) = request.lease_id.as_deref()
        && let Err(message) = validate_identifier("lease_id", lease_id)
    {
        return api_error(
            started,
            StatusCode::BAD_REQUEST,
            "invalid_lease_id",
            message,
        );
    }
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    let actor = InputActor::from(request.actor);
    match manager.signal_as(
        &session_id,
        actor.clone(),
        request.lease_id.as_deref(),
        request.signal.into(),
    ) {
        Ok(()) => result_response(
            started,
            &SignalAck {
                delivered: true,
                signal: request.signal,
                actor: actor.into(),
            },
        ),
        Err(error) => terminal_error(started, error),
    }
}

async fn list_input_leases(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Response {
    let started = Instant::now();
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    match manager.leases(&session_id) {
        Ok(leases) => result_response(
            started,
            &InputLeaseListResult {
                leases: leases.into_iter().map(Into::into).collect(),
            },
        ),
        Err(error) => terminal_error(started, error),
    }
}

async fn acquire_input_lease(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    body: Result<Json<AcquireInputLeaseRequest>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let request = match json_body(started, body) {
        Ok(request) => request,
        Err(error) => return error.into_response(),
    };
    if let Err(message) = validate_actor(&request.actor) {
        return api_error(started, StatusCode::BAD_REQUEST, "invalid_actor", message);
    }
    if let Some(lease_id) = request.lease_id.as_deref()
        && let Err(message) = validate_identifier("lease_id", lease_id)
    {
        return api_error(
            started,
            StatusCode::BAD_REQUEST,
            "invalid_lease_id",
            message,
        );
    }
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    match manager.acquire_lease(
        &session_id,
        request.actor.into(),
        request.mode.into(),
        request.ttl_ms,
        request.lease_id.as_deref(),
    ) {
        Ok(lease) => result_response(started, &InputLeaseResult::from(lease)),
        Err(error) => terminal_error(started, error),
    }
}

async fn release_input_lease(
    State(state): State<ApiState>,
    Path((session_id, lease_id)): Path<(String, String)>,
) -> Response {
    let started = Instant::now();
    if let Err(message) = validate_identifier("lease_id", &lease_id) {
        return api_error(
            started,
            StatusCode::BAD_REQUEST,
            "invalid_lease_id",
            message,
        );
    }
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    match manager.release_lease(&session_id, &lease_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => terminal_error(started, error),
    }
}

async fn close_session(State(state): State<ApiState>, Path(session_id): Path<String>) -> Response {
    let started = Instant::now();
    let mut manager = match lock_manager(started, &state) {
        Ok(manager) => manager,
        Err(error) => return error.into_response(),
    };
    match manager.close(&session_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => terminal_error(started, error),
    }
}

async fn stream_events(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let started = Instant::now();
    let last_event_id = headers
        .get(&LAST_EVENT_ID)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let subscription = {
        let manager = match lock_manager(started, &state) {
            Ok(manager) => manager,
            Err(error) => return error.into_response(),
        };
        manager.subscribe_events()
    };
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(128);
    tokio::task::spawn_blocking(move || {
        for envelope in subscription
            .backlog
            .into_iter()
            .filter(|envelope| envelope.seq > last_event_id)
        {
            if !send_sse_event(&sender, envelope) {
                return;
            }
        }
        while !sender.is_closed() {
            match subscription
                .receiver
                .recv_timeout(Duration::from_millis(250))
            {
                Ok(envelope) => {
                    if !send_sse_event(&sender, envelope) {
                        return;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });

    Sse::new(ReceiverStream::new(receiver))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// One stream item, described by the operation's `itemSchema`.
///
/// A stream item is not a finite result, so it does not carry the AFDATA
/// result envelope — but it is still a serialization boundary, so it is still
/// redacted.
fn send_sse_event(
    sender: &tokio::sync::mpsc::Sender<Result<SseEvent, Infallible>>,
    envelope: EventEnvelope,
) -> bool {
    let seq = envelope.seq;
    let payload = EventEnvelopeResult::from(envelope);
    let Ok(payload) = serde_json::to_value(&payload) else {
        return true;
    };
    let Ok(data) = serde_json::to_string(&agent_first_data::redacted_value(&payload)) else {
        return true;
    };
    sender
        .blocking_send(Ok(SseEvent::default().id(seq.to_string()).data(data)))
        .is_ok()
}

fn validate_open_request(request: &OpenSessionRequest) -> Result<(), String> {
    validate_session_id(&request.session_id)?;
    validate_dimensions(request.rows, request.cols)?;
    if request
        .program
        .as_ref()
        .is_some_and(|program| program.is_empty())
    {
        return Err("program must not be empty".to_string());
    }
    if request
        .cwd_path
        .as_ref()
        .is_some_and(|cwd_path| cwd_path.is_empty())
    {
        return Err("cwd_path must not be empty".to_string());
    }
    if request
        .title
        .as_ref()
        .is_some_and(|title| title.len() > 256)
    {
        return Err("title must be at most 256 bytes".to_string());
    }
    Ok(())
}

pub(super) fn validate_session_id(session_id: &str) -> Result<(), String> {
    validate_identifier("session_id", session_id)
}

/// Check the actor a request declared for itself.
///
/// Every protected route shares one all-powerful bearer, and `kind` arrives in
/// the request body — so a token holder could call itself `human` and get the
/// properties reserved for a person: preempting an exclusive lease, and ending
/// a secret-input window that is supposed to close only for whoever is at the
/// keyboard. The contract described that as an isolation boundary while it was
/// a self-declaration.
///
/// It is not one a request may make now. `human` is produced by the local UI
/// runtime, which the server constructs itself and never reads off the wire.
/// Everything else is a cooperative label between mutually-trusting callers of
/// the same token, which is what it always was.
fn validate_actor(actor: &ActorModel) -> Result<(), String> {
    if matches!(actor.kind, ActorKindName::Human) {
        return Err(
            "actor.kind `human` is not a claim a request may make: it is issued by the local \
             interface, where a person is actually present. Use `agent`, `controller`, or \
             `renderer`."
                .to_string(),
        );
    }
    validate_identifier("actor.id", &actor.id)
}

fn validate_identifier(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 128 {
        return Err(format!("{field} must contain 1-128 ASCII characters"));
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err(format!("{field} must not be empty"));
    };
    if !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "{field} must start with an ASCII letter or digit and contain only letters, digits, dot, underscore, or hyphen"
        ));
    }
    Ok(())
}

pub(super) fn validate_dimensions(rows: u16, cols: u16) -> Result<(), String> {
    if rows < MIN_TERMINAL_DIMENSION
        || cols < MIN_TERMINAL_DIMENSION
        || rows > MAX_TERMINAL_DIMENSION
        || cols > MAX_TERMINAL_DIMENSION
    {
        return Err(format!(
            "rows and cols must each be between {MIN_TERMINAL_DIMENSION} and \
             {MAX_TERMINAL_DIMENSION}"
        ));
    }
    Ok(())
}

fn session_info(
    started: Instant,
    manager: &mut TerminalSessionManager,
    session_id: &str,
) -> Result<SessionInfo, ApiFailure> {
    let _status = manager.status(session_id);
    manager
        .metadata(session_id)
        .map(|meta| SessionInfo::from_meta(session_id.to_string(), meta))
        .ok_or_else(|| {
            ApiFailure::new(
                started,
                StatusCode::NOT_FOUND,
                "session_not_found",
                format!("terminal session `{session_id}` not found"),
            )
        })
}

fn lock_manager(
    started: Instant,
    state: &ApiState,
) -> Result<MutexGuard<'_, TerminalSessionManager>, ApiFailure> {
    state.manager.lock().map_err(|_| {
        ApiFailure::new(
            started,
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime_lock_poisoned",
            "terminal runtime state is unavailable",
        )
    })
}

fn json_body<T>(started: Instant, body: Result<Json<T>, JsonRejection>) -> Result<T, ApiFailure> {
    body.map(|Json(value)| value).map_err(|error| {
        let status = error.status();
        let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "request_body_too_large"
        } else {
            "invalid_json"
        };
        ApiFailure::new(started, status, code, error.body_text())
    })
}

pub(super) fn terminal_error(started: Instant, error: TerminalError) -> Response {
    match error {
        TerminalError::NotFound(session_id) => api_error(
            started,
            StatusCode::NOT_FOUND,
            "session_not_found",
            format!("terminal session `{session_id}` not found"),
        ),
        TerminalError::AlreadyOpen(session_id) => api_error(
            started,
            StatusCode::CONFLICT,
            "session_already_open",
            format!("terminal session `{session_id}` is already open"),
        ),
        TerminalError::NotRunning(session_id) => api_error(
            started,
            StatusCode::CONFLICT,
            "session_not_running",
            format!("terminal session `{session_id}` is not running"),
        ),
        TerminalError::UnsupportedSignal(signal) => api_error(
            started,
            StatusCode::NOT_IMPLEMENTED,
            "signal_not_supported",
            format!("terminal signal `{signal}` is not supported on this platform"),
        ),
        TerminalError::InputLeaseRequired { session_id, actor } => api_error(
            started,
            StatusCode::CONFLICT,
            "input_lease_required",
            format!("input actor `{actor}` requires a lease for terminal session `{session_id}`"),
        ),
        TerminalError::InputLeaseNotFound {
            session_id,
            lease_id,
        } => api_error(
            started,
            StatusCode::NOT_FOUND,
            "input_lease_not_found",
            format!("input lease `{lease_id}` was not found for terminal session `{session_id}`"),
        ),
        TerminalError::InputLeaseConflict {
            session_id,
            actor,
            held_by,
        } => {
            let holder = held_by
                .map(|holder| format!(" held by `{holder}`"))
                .unwrap_or_default();
            ApiFailure::new(
                started,
                StatusCode::CONFLICT,
                "input_lease_conflict",
                format!(
                    "input actor `{actor}` conflicts with the active lease for terminal session `{session_id}`{holder}"
                ),
            )
            .retryable()
            .into_response()
        }
        TerminalError::SecretInputActive { session_id, actor } => ApiFailure::new(
            started,
            StatusCode::CONFLICT,
            "secret_input_active",
            format!(
                "terminal session `{session_id}` is in secret input mode; actor `{actor}` is suspended"
            ),
        )
        .retryable()
        .hint("wait for the secret_input_ended event on the event stream")
        .into_response(),
        TerminalError::SecretInputExitDenied { session_id, actor } => ApiFailure::new(
            started,
            StatusCode::FORBIDDEN,
            "secret_input_exit_denied",
            format!(
                "actor `{actor}` may not end secret input mode on terminal session `{session_id}`"
            ),
        )
        .hint("only a human actor ends secret input mode; ask the person at the terminal")
        .into_response(),
        TerminalError::SecretInputSettling {
            session_id,
            quiet_for_ms,
        } => ApiFailure::new(
            started,
            StatusCode::CONFLICT,
            "secret_input_settling",
            format!(
                "terminal session `{session_id}` is still producing output ({quiet_for_ms}ms quiet); secret input mode cannot end yet"
            ),
        )
        .retryable()
        .hint("repeat the end action once the session stops producing output")
        .into_response(),
        TerminalError::InvalidSecretInputReason(message) => api_error(
            started,
            StatusCode::BAD_REQUEST,
            "invalid_secret_input_reason",
            message,
        ),
        TerminalError::InvalidInputLeaseTtl { ttl_ms } => api_error(
            started,
            StatusCode::BAD_REQUEST,
            "invalid_lease_ttl",
            format!("input lease ttl_ms `{ttl_ms}` is outside the supported range"),
        ),
        TerminalError::InvalidDimensions { rows, cols } => api_error(
            started,
            StatusCode::BAD_REQUEST,
            "invalid_dimensions",
            format!(
                "terminal rows `{rows}` and cols `{cols}` must each be between \
                 {MIN_TERMINAL_DIMENSION} and {MAX_TERMINAL_DIMENSION}"
            ),
        ),
        TerminalError::InvalidInputActor(message) => {
            api_error(started, StatusCode::BAD_REQUEST, "invalid_actor", message)
        }
        TerminalError::Poisoned => api_error(
            started,
            StatusCode::INTERNAL_SERVER_ERROR,
            "runtime_lock_poisoned",
            "terminal session state is unavailable",
        ),
        TerminalError::Io(error) => api_error(
            started,
            StatusCode::INTERNAL_SERVER_ERROR,
            "terminal_io_error",
            error.to_string(),
        ),
        TerminalError::Backend(message) => api_error(
            started,
            StatusCode::INTERNAL_SERVER_ERROR,
            "terminal_backend_error",
            message,
        ),
    }
}

pub(super) fn api_error(
    started: Instant,
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> Response {
    ApiFailure::new(started, status, code, message).into_response()
}

async fn require_bearer(
    State(state): State<ApiState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .and_then(|value| value.strip_prefix(b"Bearer "));
    let authorized =
        supplied.is_some_and(|value| constant_time_eq(value, state.access_token_secret.as_slice()));
    if !authorized {
        return api_error(
            Instant::now(),
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "a valid bearer credential is required",
        );
    }
    next.run(request).await
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn not_found() -> Response {
    api_error(
        Instant::now(),
        StatusCode::NOT_FOUND,
        "api_route_not_found",
        "API route not found",
    )
}

async fn method_not_allowed() -> Response {
    api_error(
        Instant::now(),
        StatusCode::METHOD_NOT_ALLOWED,
        "api_method_not_allowed",
        "HTTP method is not allowed for this route",
    )
}

/// The authority every in-crate test serves this API under.
///
/// A UI request naming anything else is refused, so tests that drive `/ui`
/// have to say this in a `Host` header the way a browser would — which is the
/// check itself being exercised rather than worked around.
#[cfg(test)]
pub(super) const TEST_API_AUTHORITY: &str = "127.0.0.1:9418";

#[cfg(test)]
pub(super) fn test_router(state: ApiState) -> Router {
    router(
        state,
        Arc::new(
            ui::TerminalUi::resolve(std::path::Path::new("/nonexistent"))
                .expect("afterminal's own terminal page renders"),
        ),
        &format!("http://{TEST_API_AUTHORITY}"),
    )
    .expect("the API mounts its UI")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use axum::body::{Body, to_bytes};
    use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
    use axum::http::{Request, StatusCode};
    use base64::Engine;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::{ApiState, result_response, test_router};
    use crate::test_shell;

    const TOKEN: &str = "test-token-0123456789-abcdefghijkl";

    /// A shared lease for a caller that is not a person.
    ///
    /// Every actor except `human` needs one to write input or send a signal,
    /// and `human` is not something a request may call itself — so this is the
    /// ordinary shape of an API caller now, and the tests say so.
    async fn lease_for(app: &axum::Router, session_id: &str, actor_id: &str) -> String {
        let lease = call_json(
            app,
            "POST",
            &format!("/v1/sessions/{session_id}/leases"),
            json!({
                "actor": {"kind": "controller", "id": actor_id},
                "mode": "shared",
                "ttl_ms": 60_000
            }),
        )
        .await;
        assert_eq!(lease.0, StatusCode::OK, "{}", lease.1);
        lease.1["lease_id"].as_str().expect("lease id").to_string()
    }

    /// What the local interface issues, and what no request body can produce.
    fn ui_test_actor() -> crate::InputActor {
        crate::InputActor {
            kind: crate::InputActorKind::Human,
            id: "local-ui".to_string(),
        }
    }

    #[tokio::test]
    async fn discovery_is_public_and_sessions_require_authentication() {
        let app = test_router(ApiState::new(TOKEN.to_string()));
        let openapi = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(openapi.status(), StatusCode::OK);
        assert_eq!(
            openapi
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/vnd.oai.openapi+json;version=3.2")
        );

        let sessions = app
            .oneshot(
                Request::builder()
                    .uri("/v1/sessions")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(sessions.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_controls_and_reads_two_independent_terminal_sessions() {
        let app = test_router(ApiState::new(TOKEN.to_string()));
        for session_id in ["api_a", "api_b"] {
            let response = call_json(
                &app,
                "POST",
                "/v1/sessions",
                json!({
                    "session_id": session_id,
                    "program": test_shell::program(),
                    "args": test_shell::args(),
                    "rows": 24,
                    "cols": 80
                }),
            )
            .await;
            assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        }

        for (session_id, marker) in [("api_a", "MARKER_A"), ("api_b", "MARKER_B")] {
            // A lease, because this caller is not a person. Writing without one
            // used to be reachable by declaring `kind:"human"`, which is the
            // privilege a request body can no longer grant itself.
            let lease = call_json(
                &app,
                "POST",
                &format!("/v1/sessions/{session_id}/leases"),
                json!({
                    "actor": {"kind": "controller", "id": "api-controller"},
                    "mode": "shared",
                    "ttl_ms": 60_000
                }),
            )
            .await;
            assert_eq!(lease.0, StatusCode::OK, "{}", lease.1);
            let lease_id = lease.1["lease_id"].as_str().expect("lease id").to_string();

            let command = test_shell::echo(marker);
            let data_base64 = base64::engine::general_purpose::STANDARD.encode(&command);
            let response = call_json(
                &app,
                "POST",
                &format!("/v1/sessions/{session_id}/input"),
                json!({
                    "actor": {"kind": "controller", "id": "api-controller"},
                    "lease_id": lease_id,
                    "data_base64": data_base64
                }),
            )
            .await;
            assert_eq!(response.0, StatusCode::OK, "{}", response.1);
            assert_eq!(response.1["accepted"], true);
            assert_eq!(
                response.1["input_bytes"],
                u64::try_from(command.len()).expect("input length")
            );
        }

        for (session_id, marker) in [("api_a", "MARKER_A"), ("api_b", "MARKER_B")] {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let response =
                    call_empty(&app, "GET", &format!("/v1/sessions/{session_id}/screen")).await;
                assert_eq!(response.0, StatusCode::OK, "{}", response.1);
                let lines = response.1["lines"]
                    .as_array()
                    .expect("screen lines")
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n");
                if lines.contains(marker) {
                    break;
                }
                assert!(Instant::now() < deadline, "missing {marker}: {lines}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        let listed = call_empty(&app, "GET", "/v1/sessions").await;
        assert_eq!(listed.0, StatusCode::OK);
        let ids = listed.1["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .filter_map(|session| session["session_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["api_a", "api_b"]);

        for session_id in ["api_a", "api_b"] {
            let closed = call_empty(&app, "DELETE", &format!("/v1/sessions/{session_id}")).await;
            assert_eq!(closed.0, StatusCode::NO_CONTENT);
        }
    }

    #[tokio::test]
    async fn agents_share_input_and_no_bearer_can_preempt_an_exclusive_lease() {
        let app = test_router(ApiState::new(TOKEN.to_string()));
        let session_id = "api_multi_actor";
        let opened = call_json(
            &app,
            "POST",
            "/v1/sessions",
            json!({
                "session_id": session_id,
                "program": test_shell::program(),
                "args": test_shell::args(),
                "rows": 24,
                "cols": 80
            }),
        )
        .await;
        assert_eq!(opened.0, StatusCode::OK, "{}", opened.1);

        let lease_a = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/leases"),
            json!({
                "actor": {"kind": "agent", "id": "agent-a"},
                "mode": "shared",
                "ttl_ms": 60_000
            }),
        )
        .await;
        assert_eq!(lease_a.0, StatusCode::OK, "{}", lease_a.1);
        let lease_a_id = lease_a.1["lease_id"]
            .as_str()
            .expect("agent A lease id")
            .to_string();

        let lease_b = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/leases"),
            json!({
                "actor": {"kind": "agent", "id": "agent-b"},
                "mode": "shared",
                "ttl_ms": 60_000
            }),
        )
        .await;
        assert_eq!(lease_b.0, StatusCode::OK, "{}", lease_b.1);
        let lease_b_id = lease_b.1["lease_id"]
            .as_str()
            .expect("agent B lease id")
            .to_string();

        for (actor_id, lease_id, marker) in [
            ("agent-a", lease_a_id.as_str(), "API_AGENT_A"),
            ("agent-b", lease_b_id.as_str(), "API_AGENT_B"),
        ] {
            let command = test_shell::echo(marker);
            let input = call_json(
                &app,
                "POST",
                &format!("/v1/sessions/{session_id}/input"),
                json!({
                    "actor": {"kind": "agent", "id": actor_id},
                    "lease_id": lease_id,
                    "data_base64": base64::engine::general_purpose::STANDARD
                        .encode(&command)
                }),
            )
            .await;
            assert_eq!(input.0, StatusCode::OK, "{}", input.1);
            assert_eq!(input.1["actor"]["id"], actor_id);
            wait_for_screen_text(&app, session_id, marker).await;
        }

        let listed = call_empty(&app, "GET", &format!("/v1/sessions/{session_id}/leases")).await;
        assert_eq!(listed.0, StatusCode::OK, "{}", listed.1);
        assert_eq!(
            listed.1["leases"].as_array().expect("input leases").len(),
            2
        );

        let conflict = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/leases"),
            json!({
                "actor": {"kind": "agent", "id": "agent-a"},
                "mode": "exclusive",
                "ttl_ms": 60_000,
                "lease_id": lease_a_id.as_str()
            }),
        )
        .await;
        assert_eq!(conflict.0, StatusCode::CONFLICT, "{}", conflict.1);
        assert_eq!(conflict.1["error"]["code"], "input_lease_conflict");

        let released = call_empty(
            &app,
            "DELETE",
            &format!("/v1/sessions/{session_id}/leases/{lease_b_id}"),
        )
        .await;
        assert_eq!(released.0, StatusCode::NO_CONTENT, "{}", released.1);

        let exclusive = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/leases"),
            json!({
                "actor": {"kind": "agent", "id": "agent-a"},
                "mode": "exclusive",
                "ttl_ms": 60_000,
                "lease_id": lease_a_id.as_str()
            }),
        )
        .await;
        assert_eq!(exclusive.0, StatusCode::OK, "{}", exclusive.1);

        // Preempting an exclusive lease is a person's privilege, and this is
        // where it used to be available to anyone holding the bearer: `human`
        // was a string in the request body. It is refused now, so the exclusive
        // lease actually holds against every caller of this API.
        let claimed_human = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/input"),
            json!({
                "actor": {"kind": "human", "id": "human-a"},
                "data_base64": base64::engine::general_purpose::STANDARD
                    .encode(test_shell::echo("API_HUMAN"))
            }),
        )
        .await;
        assert_eq!(
            claimed_human.0,
            StatusCode::BAD_REQUEST,
            "a bearer must not be able to call itself human: {}",
            claimed_human.1
        );

        // The exclusive lease survived the attempt, so its holder still writes.
        let still_held = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/input"),
            json!({
                "actor": {"kind": "agent", "id": "agent-a"},
                "lease_id": lease_a_id.as_str(),
                "data_base64": base64::engine::general_purpose::STANDARD
                    .encode(test_shell::echo("STILL_HELD"))
            }),
        )
        .await;
        assert_eq!(still_held.0, StatusCode::OK, "{}", still_held.1);
        wait_for_screen_text(&app, session_id, "STILL_HELD").await;

        let listed = call_empty(&app, "GET", &format!("/v1/sessions/{session_id}/leases")).await;
        assert_eq!(
            listed.1["leases"].as_array().expect("input leases").len(),
            1,
            "nothing over this API can take an exclusive lease away"
        );

        let closed = call_empty(&app, "DELETE", &format!("/v1/sessions/{session_id}")).await;
        assert_eq!(closed.0, StatusCode::NO_CONTENT);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_delivers_interrupt_terminate_and_kill_signals() {
        let app = test_router(ApiState::new(TOKEN.to_string()));
        for (session_id, signal, trap_name, marker) in [
            ("api_signal_int", "interrupt", "INT", "API_SIGNAL_INT"),
            ("api_signal_term", "terminate", "TERM", "API_SIGNAL_TERM"),
        ] {
            let command = format!(
                "trap 'printf \"{marker}\\n\"; exit 0' {trap_name}; \
                 printf 'API_SIGNAL_READY\\n'; while :; do sleep 1; done"
            );
            let opened = call_json(
                &app,
                "POST",
                "/v1/sessions",
                json!({
                    "session_id": session_id,
                    "program": "/bin/sh",
                    "args": ["-c", command]
                }),
            )
            .await;
            assert_eq!(opened.0, StatusCode::OK, "{}", opened.1);
            wait_for_screen_text(&app, session_id, "API_SIGNAL_READY").await;

            let lease_id = lease_for(&app, session_id, "api-controller").await;
            let delivered = call_json(
                &app,
                "POST",
                &format!("/v1/sessions/{session_id}/signal"),
                json!({
                    "actor": {"kind": "controller", "id": "api-controller"},
                    "lease_id": lease_id,
                    "signal": signal
                }),
            )
            .await;
            assert_eq!(delivered.0, StatusCode::OK, "{}", delivered.1);
            assert_eq!(delivered.1["delivered"], true);
            assert_eq!(delivered.1["signal"], signal);
            wait_for_screen_text(&app, session_id, marker).await;

            let closed = call_empty(&app, "DELETE", &format!("/v1/sessions/{session_id}")).await;
            assert_eq!(closed.0, StatusCode::NO_CONTENT);
        }

        let session_id = "api_signal_kill";
        let opened = call_json(
            &app,
            "POST",
            "/v1/sessions",
            json!({
                "session_id": session_id,
                "program": "/bin/sh",
                "args": [
                    "-c",
                    "printf 'API_SIGNAL_READY\\n'; while :; do sleep 1; done"
                ]
            }),
        )
        .await;
        assert_eq!(opened.0, StatusCode::OK, "{}", opened.1);
        wait_for_screen_text(&app, session_id, "API_SIGNAL_READY").await;

        let lease_id = lease_for(&app, session_id, "api-controller").await;
        let delivered = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/signal"),
            json!({
                "actor": {"kind": "controller", "id": "api-controller"},
                "lease_id": lease_id,
                "signal": "kill"
            }),
        )
        .await;
        assert_eq!(delivered.0, StatusCode::OK, "{}", delivered.1);
        assert_eq!(delivered.1["delivered"], true);
        assert_eq!(delivered.1["signal"], "kill");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let session = call_empty(&app, "GET", &format!("/v1/sessions/{session_id}")).await;
            assert_eq!(session.0, StatusCode::OK, "{}", session.1);
            if session.1["status"] == "exited" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "kill signal did not terminate the process: {}",
                session.1
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let closed = call_empty(&app, "DELETE", &format!("/v1/sessions/{session_id}")).await;
        assert_eq!(closed.0, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn discovery_serves_the_schema_index_and_its_schemas() {
        let app = test_router(ApiState::new(TOKEN.to_string()));
        let index = fetch_public(&app, "/schemas/index.json").await;
        assert_eq!(index.0, StatusCode::OK);
        let entries = index.1["schemas"].as_array().expect("schemas").clone();
        assert_eq!(index.1["count"], entries.len());
        assert!(!entries.is_empty());
        for entry in entries {
            let url = entry["schema_url"].as_str().expect("schema url");
            let schema = fetch_public(&app, url).await;
            assert_eq!(schema.0, StatusCode::OK, "{url}");
            assert_eq!(schema.1["title"], entry["component_name"], "{url}");
            assert!(schema.1["$id"].is_string(), "{url}");
        }
        let missing = fetch_public(&app, "/schemas/not-a-schema.schema.json").await;
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
        assert_eq!(missing.1["error"]["code"], "schema_not_found");
    }

    /// The redaction call at the serialization boundary is the reason a
    /// `_secret`-suffixed field cannot leave this process. Nothing in the
    /// contract is named that way today, which is exactly why this asserts on
    /// the boundary itself rather than on a payload that might stop having one.
    #[tokio::test]
    async fn the_serialization_boundary_blanks_secret_named_fields() {
        let started = Instant::now();
        let response = result_response(
            started,
            &json!({"visible": "kept", "probe_token_secret": "sk-live-do-not-publish"}),
        );
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body");
        let value: Value = serde_json::from_slice(&bytes).expect("JSON response");
        assert_eq!(value["result"]["visible"], "kept");
        assert_eq!(value["result"]["probe_token_secret"], "***");
        assert!(!String::from_utf8_lossy(&bytes).contains("sk-live-do-not-publish"));
    }

    #[tokio::test]
    async fn secret_input_suspends_agents_and_withholds_the_screen() {
        let state = ApiState::new(TOKEN.to_string());
        let app = test_router(state.clone());
        let session_id = "api_secret";
        let opened = call_json(
            &app,
            "POST",
            "/v1/sessions",
            json!({
                "session_id": session_id,
                "program": test_shell::program(),
                "args": test_shell::args(),
            }),
        )
        .await;
        assert_eq!(opened.0, StatusCode::OK, "{}", opened.1);
        assert_eq!(opened.1["secret_input"], false);

        let lease = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/leases"),
            json!({"actor": {"kind": "agent", "id": "agent-a"}, "mode": "shared"}),
        )
        .await;
        assert_eq!(lease.0, StatusCode::OK, "{}", lease.1);
        let lease_id = lease.1["lease_id"].as_str().expect("lease id").to_string();

        let started = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/secret-input/actions"),
            json!({
                "action": "start",
                "actor": {"kind": "controller", "id": "api-controller"},
                "reason": "password prompt"
            }),
        )
        .await;
        assert_eq!(started.0, StatusCode::OK, "{}", started.1);
        assert_eq!(started.1["secret_input"], true);
        assert_eq!(started.1["reason"], "password prompt");

        // The agent is refused input, and told when to come back.
        let refused = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/input"),
            json!({
                "actor": {"kind": "agent", "id": "agent-a"},
                "lease_id": lease_id,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(b"whoami\n")
            }),
        )
        .await;
        assert_eq!(refused.0, StatusCode::CONFLICT, "{}", refused.1);
        assert_eq!(refused.1["error"]["code"], "secret_input_active");
        assert_eq!(refused.1["error"]["retryable"], true);
        assert!(refused.1["error"]["hint"].is_string());

        // And refused a new lease, and refused the right to switch it off.
        let refused_lease = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/leases"),
            json!({"actor": {"kind": "agent", "id": "agent-b"}, "mode": "shared"}),
        )
        .await;
        assert_eq!(refused_lease.0, StatusCode::CONFLICT, "{}", refused_lease.1);
        let refused_exit = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/secret-input/actions"),
            json!({"action": "end", "actor": {"kind": "agent", "id": "agent-a"}}),
        )
        .await;
        assert_eq!(refused_exit.0, StatusCode::FORBIDDEN, "{}", refused_exit.1);
        assert_eq!(refused_exit.1["error"]["code"], "secret_input_exit_denied");

        // Nothing reaching this API can type into an open window, because
        // nothing reaching this API is the person: `human` is issued by the
        // local interface, not claimed in a request body.
        let typed = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/input"),
            json!({
                "actor": {"kind": "controller", "id": "api-controller"},
                "data_base64": base64::engine::general_purpose::STANDARD
                    .encode(test_shell::echo("API_SECRET_VALUE"))
            }),
        )
        .await;
        assert_eq!(
            typed.0,
            StatusCode::CONFLICT,
            "an agent must not write into a secret window: {}",
            typed.1
        );
        // The person types, through the channel a person actually has.
        {
            let mut manager = state.manager.lock().expect("manager lock");
            manager
                .write_as(
                    session_id,
                    ui_test_actor(),
                    None,
                    &test_shell::echo("SECRET_VALUE_XYZ"),
                )
                .expect("the local interface writes into its own window");
        }

        let screen = call_empty(&app, "GET", &format!("/v1/sessions/{session_id}/screen")).await;
        assert_eq!(screen.0, StatusCode::OK, "{}", screen.1);
        assert_eq!(screen.1["secret_input"], true);
        assert_eq!(screen.1["lines"], json!([]));
        let listed = call_empty(&app, "GET", "/v1/sessions").await;
        assert_eq!(listed.1["sessions"][0]["secret_input"], true);

        // And nothing over this API ends it. The skill has always told agents
        // "you cannot turn it off yourself, by design"; before, a request body
        // saying `kind:"human"` could.
        let refused = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/secret-input/actions"),
            json!({"action": "end", "actor": {"kind": "controller", "id": "api-controller"}}),
        )
        .await;
        assert_eq!(refused.1["error"]["code"], "secret_input_exit_denied");
        let claimed = call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/secret-input/actions"),
            json!({"action": "end", "actor": {"kind": "human", "id": "api-human"}}),
        )
        .await;
        assert_eq!(
            claimed.0,
            StatusCode::BAD_REQUEST,
            "a bearer must not close a secret window by calling itself human: {}",
            claimed.1
        );

        // The window closes through the local interface, where a person is.
        // The guard is taken and dropped inside each attempt so it is never
        // held across the wait between them.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let attempt = {
                let mut manager = state.manager.lock().expect("manager lock");
                manager.exit_secret(session_id, ui_test_actor())
            };
            match attempt {
                Ok(status) => {
                    assert!(!status.active);
                    break;
                }
                Err(crate::TerminalError::SecretInputSettling { .. }) => {
                    assert!(Instant::now() < deadline, "the session never settled");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(other) => panic!("unexpected error ending secret input: {other}"),
            }
        }

        // The live parser saw the protected interval, but that grid is
        // discarded before publication resumes.
        let resumed = call_empty(&app, "GET", &format!("/v1/sessions/{session_id}/screen")).await;
        assert_eq!(resumed.0, StatusCode::OK);
        assert!(
            !resumed.1["lines"]
                .as_array()
                .expect("screen lines")
                .iter()
                .filter_map(Value::as_str)
                .any(|line| line.contains("SECRET_VALUE_XYZ")),
            "the secret the person typed must not reappear when publication resumes"
        );
        let closed = call_empty(&app, "DELETE", &format!("/v1/sessions/{session_id}")).await;
        assert_eq!(closed.0, StatusCode::NO_CONTENT);
    }

    /// Resuming matters most where afterminal is about to be reached from: a
    /// phone through a reverse proxy, where the stream drops and reconnects.
    #[tokio::test]
    async fn the_event_stream_resumes_after_last_event_id() {
        use tokio_stream::StreamExt;

        let app = test_router(ApiState::new(TOKEN.to_string()));
        let session_id = "api_resume";
        let opened = call_json(
            &app,
            "POST",
            "/v1/sessions",
            json!({
                "session_id": session_id,
                "program": test_shell::program(),
                "args": test_shell::args(),
            }),
        )
        .await;
        assert_eq!(opened.0, StatusCode::OK, "{}", opened.1);
        let lease_id = lease_for(&app, session_id, "api-controller").await;
        call_json(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/input"),
            json!({
                "actor": {"kind": "controller", "id": "api-controller"},
                "lease_id": lease_id,
                "data_base64": base64::engine::general_purpose::STANDARD
                    .encode(test_shell::echo("RESUME"))
            }),
        )
        .await;
        wait_for_screen_text(&app, session_id, "RESUME").await;

        // Ask for everything after the session-opened event; the backlog holds
        // it, so the replay must start strictly after it.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/events")
                    .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header("last-event-id", "1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let frame = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("an SSE frame")
            .expect("a stream item")
            .expect("frame bytes");
        let text = String::from_utf8_lossy(&frame).into_owned();
        let id = text
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .and_then(|id| id.trim().parse::<u64>().ok())
            .unwrap_or_else(|| panic!("no SSE id in {text:?}"));
        assert!(id > 1, "resumed at {id}, replaying an event already seen");
        assert!(text.contains("\"session_id\":\"api_resume\""), "{text}");

        let closed = call_empty(&app, "DELETE", &format!("/v1/sessions/{session_id}")).await;
        assert_eq!(closed.0, StatusCode::NO_CONTENT);
    }

    async fn fetch_public(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("response body");
        (
            status,
            serde_json::from_slice(&bytes).expect("JSON response"),
        )
    }

    async fn wait_for_screen_text(app: &axum::Router, session_id: &str, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let response =
                call_empty(app, "GET", &format!("/v1/sessions/{session_id}/screen")).await;
            assert_eq!(response.0, StatusCode::OK, "{}", response.1);
            let lines = response.1["lines"]
                .as_array()
                .expect("screen lines")
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n");
            if lines.contains(marker) {
                return;
            }
            assert!(Instant::now() < deadline, "missing {marker}: {lines}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn call_json(
        app: &axum::Router,
        method: &str,
        uri: &str,
        payload: Value,
    ) -> (StatusCode, Value) {
        call(
            app,
            Request::builder()
                .method(method)
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&payload).expect("serialize request"),
                ))
                .expect("request"),
        )
        .await
    }

    async fn call_empty(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
        call(
            app,
            Request::builder()
                .method(method)
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
    }

    /// Every call in this module goes through here, so the envelope is checked
    /// on every domain response rather than in one test that could rot: a
    /// success must be an AFDATA result envelope, and what the callers below
    /// then index into is its `result`.
    async fn call(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("JSON response")
        };
        if status.is_success() && !value.is_null() {
            assert_eq!(value["kind"], "result", "not a result envelope: {value}");
            assert!(
                value["trace"]["duration_ms"].is_number(),
                "no trace: {value}"
            );
            return (status, value["result"].clone());
        }
        if !value.is_null() {
            assert_eq!(value["kind"], "error", "not an error envelope: {value}");
            assert!(value["error"]["retryable"].is_boolean(), "{value}");
        }
        (status, value)
    }
}
