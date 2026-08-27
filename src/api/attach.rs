use std::time::Duration;

use agent_first_ui::{UiCredentialLease, is_ui_credential};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ApiSetupError;

const UI_ATTACH_REQUEST_TIMEOUT_S: u64 = 15;

#[derive(Serialize)]
struct CreateUiAttachmentRequest<'a> {
    initial_session_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct CreateUiAttachmentResult {
    ui_access_url_secret: String,
    ui_access_idle_timeout_ms: u64,
}

/// A private UI capability issued by an already-running terminal API.
///
/// The API bearer and UI capability are intentionally retained only in this
/// process and must never be emitted through ordinary CLI output.
pub struct RemoteUiAttachment {
    client: Client,
    api_url: Url,
    api_access_token_secret: String,
    /// The URL the API handed back, authority and all. Not assembled here:
    /// the issuing side pins the name it answers under, and this side only
    /// knows the one it happened to dial.
    ui_access_url_secret: Url,
    ui_access_token_secret: String,
    lease: UiCredentialLease,
}

impl RemoteUiAttachment {
    /// Return the normalized API URL without either credential.
    pub fn api_url(&self) -> String {
        self.api_url.as_str().trim_end_matches('/').to_string()
    }

    /// The private browser URL, optionally selecting one existing terminal
    /// session when the page opens.
    pub fn browser_url(&self, initial_session_id: Option<&str>) -> Result<String, ApiSetupError> {
        let mut url = self.ui_access_url_secret.clone();
        if let Some(session_id) = initial_session_id {
            url.query_pairs_mut()
                .append_pair("initial_session_id", session_id);
        }
        Ok(url.to_string())
    }

    /// Revoke this browser capability. An already-expired capability counts as
    /// revoked; callers can safely ignore transport failure during shutdown.
    pub async fn revoke(&self) -> Result<bool, ApiSetupError> {
        let endpoint = self.maintenance_url()?;
        let response = self
            .client
            .delete(endpoint)
            .bearer_auth(&self.api_access_token_secret)
            .send()
            .await
            .map_err(|error| remote_request_error("revoke terminal UI attachment", error))?;
        Ok(matches!(
            response.status(),
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND
        ))
    }

    /// Keep this private upstream credential alive while AFUI owns the
    /// person-facing delivery. The idle timeout is crash cleanup only; it is
    /// never the Window, Link, or Session lifetime.
    ///
    /// Resolves only when a renewal fails: the thing that finishes is the
    /// delivery, and dropping this future is how that says so.
    pub async fn keep_alive(&self) -> Result<(), ApiSetupError> {
        Err(self.lease.keep_alive(async || self.renew().await).await)
    }

    async fn renew(&self) -> Result<(), ApiSetupError> {
        let response = self
            .client
            .put(self.maintenance_url()?)
            .bearer_auth(&self.api_access_token_secret)
            .send()
            .await
            .map_err(|error| remote_request_error("keep terminal UI attachment alive", error))?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(ApiSetupError::RemoteUi(format!(
                "terminal API rejected UI attachment keep-alive with HTTP {}",
                response.status().as_u16()
            )));
        }
        Ok(())
    }

    fn maintenance_url(&self) -> Result<Url, ApiSetupError> {
        self.api_url
            .join(&format!("ui-attachments/{}", self.ui_access_token_secret))
            .map_err(|error| {
                ApiSetupError::InvalidApiUrl(format!(
                    "build terminal UI attachment maintenance URL: {error}"
                ))
            })
    }
}

/// Ask an existing terminal API to expose its live runtime as an AFUI upstream.
/// The returned capability is independent from the API bearer.
pub async fn create_remote_ui_attachment(
    api_url: &str,
    api_access_token_secret: &str,
    initial_session_id: Option<&str>,
) -> Result<RemoteUiAttachment, ApiSetupError> {
    let api_url = normalize_api_url(api_url)?;
    let endpoint = api_url.join("ui-attachments").map_err(|error| {
        ApiSetupError::InvalidApiUrl(format!("build UI attachment URL: {error}"))
    })?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(UI_ATTACH_REQUEST_TIMEOUT_S))
        .timeout(Duration::from_secs(UI_ATTACH_REQUEST_TIMEOUT_S))
        .user_agent(concat!("afterminal/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| remote_request_error("build terminal API client", error))?;
    let response = client
        .post(endpoint)
        .bearer_auth(api_access_token_secret)
        .json(&CreateUiAttachmentRequest { initial_session_id })
        .send()
        .await
        .map_err(|error| remote_request_error("request terminal UI attachment", error))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| remote_request_error("read terminal UI attachment response", error))?;
    if !status.is_success() {
        return Err(ApiSetupError::RemoteUi(format!(
            "terminal API rejected UI attachment with HTTP {}: {}",
            status.as_u16(),
            api_error_message(&body)
        )));
    }
    let result: CreateUiAttachmentResult = serde_json::from_slice(&body).map_err(|error| {
        ApiSetupError::RemoteUi(format!(
            "terminal API returned an invalid UI attachment response: {error}"
        ))
    })?;
    let ui_access_url_secret = Url::parse(&result.ui_access_url_secret).map_err(|error| {
        ApiSetupError::RemoteUi(format!(
            "terminal API returned an unusable UI attachment URL: {error}"
        ))
    })?;
    // The credential is the last path segment, and what makes it one is AFUI's
    // to say — this side used to carry its own copy of "sixty-four hex digits".
    let ui_access_token_secret = ui_access_url_secret
        .path_segments()
        .and_then(|mut segments| {
            segments
                .rfind(|segment| !segment.is_empty())
                .map(str::to_owned)
        })
        .filter(|segment| is_ui_credential(segment))
        .ok_or_else(|| {
            ApiSetupError::RemoteUi("terminal API returned an invalid UI capability".to_string())
        })?;
    if result.ui_access_idle_timeout_ms < 3 {
        return Err(ApiSetupError::RemoteUi(
            "terminal API returned an invalid UI attachment idle timeout".to_string(),
        ));
    }
    Ok(RemoteUiAttachment {
        client,
        api_url,
        api_access_token_secret: api_access_token_secret.to_string(),
        ui_access_url_secret,
        ui_access_token_secret,
        lease: UiCredentialLease::new(Duration::from_millis(result.ui_access_idle_timeout_ms)),
    })
}

fn normalize_api_url(raw: &str) -> Result<Url, ApiSetupError> {
    let mut url = Url::parse(raw).map_err(|error| {
        ApiSetupError::InvalidApiUrl(format!("--api-url must be an absolute HTTP URL: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ApiSetupError::InvalidApiUrl(
            "--api-url must use http or https and include a host".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ApiSetupError::InvalidApiUrl(
            "--api-url must not contain credentials".to_string(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ApiSetupError::InvalidApiUrl(
            "--api-url must not contain a query or fragment".to_string(),
        ));
    }
    let normalized_path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&normalized_path);
    Ok(url)
}

fn api_error_message(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "request failed".to_string())
}

fn remote_request_error(operation: &'static str, error: reqwest::Error) -> ApiSetupError {
    ApiSetupError::RemoteUi(format!("{operation}: {}", error.without_url()))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::{create_remote_ui_attachment, normalize_api_url};
    use crate::api::{ApiState, router};

    const API_TOKEN: &str = "attach-test-token-0123456789-abcdef";

    #[test]
    fn api_url_validation_normalizes_paths_and_rejects_embedded_secrets() {
        assert_eq!(
            normalize_api_url("http://127.0.0.1:9418")
                .expect("valid URL")
                .as_str(),
            "http://127.0.0.1:9418/"
        );
        assert_eq!(
            normalize_api_url("https://example.test/terminal-api")
                .expect("valid prefixed URL")
                .as_str(),
            "https://example.test/terminal-api/"
        );
        assert!(normalize_api_url("ftp://example.test").is_err());
        assert!(normalize_api_url("https://user:secret@example.test").is_err());
        assert!(normalize_api_url("https://example.test?token_secret=x").is_err());
        assert!(normalize_api_url("https://example.test/#fragment").is_err());
    }

    #[tokio::test]
    async fn remote_client_opens_and_revokes_ui_on_running_api() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test API");
        let address = listener.local_addr().expect("test API address");
        let api_url = format!("http://{address}");
        let app = router(
            ApiState::new(API_TOKEN.to_string()),
            std::sync::Arc::new(
                crate::api::ui::TerminalUi::resolve(std::path::Path::new("/nonexistent"))
                    .expect("afterminal's own terminal page renders"),
            ),
            &api_url,
        )
        .expect("the API mounts its UI");
        let server = tokio::spawn(async move {
            let result = axum::serve(listener, app).await;
            assert!(result.is_ok(), "serve test API");
        });
        let attachment = create_remote_ui_attachment(&api_url, API_TOKEN, None)
            .await
            .expect("create remote UI attachment");
        assert_eq!(attachment.api_url(), api_url);
        let browser_url = attachment.browser_url(Some("codex")).expect("browser URL");
        assert!(browser_url.contains("initial_session_id=codex"));
        let page = reqwest::get(&browser_url).await.expect("request UI page");
        assert_eq!(page.status(), StatusCode::OK);
        assert!(attachment.revoke().await.expect("revoke UI attachment"));
        let revoked_page = reqwest::get(&browser_url)
            .await
            .expect("request revoked UI page");
        assert_eq!(revoked_page.status(), StatusCode::NOT_FOUND);
        server.abort();
    }
}
