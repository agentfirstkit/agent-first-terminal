//! The terminal UI through AFUI's real retained-state and typed-call protocol.
//!
//! AFUI's test client owns the WebSocket and its envelope. This test knows only
//! the terminal's domain action/state shapes, exactly as its page does.

#![cfg(feature = "api")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use agent_first_terminal::api::{ApiState, TerminalUi, router};
use agent_first_ui::test_support::RuntimeClient;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const TOKEN: &str = "terminal-runtime-0123456789-abcdef";
const SESSION_ID: &str = "runtime";
const SECRET: &str = "correct-horse-battery-staple-runtime";

struct TestServer {
    url: String,
    task: JoinHandle<()>,
    _workspace: TempDir,
}

impl TestServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test API");
        let address = listener.local_addr().expect("test API address");
        let url = format!("http://{address}");
        let workspace = tempfile::tempdir().expect("test workspace");
        let ui = Arc::new(TerminalUi::resolve(workspace.path()).expect("built-in terminal UI"));
        let app = router(ApiState::new(TOKEN.to_owned()), ui, &url).expect("terminal API router");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve terminal test API");
        });
        Self {
            url,
            task,
            _workspace: workspace,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn post_result(client: &Client, server: &TestServer, path: &str, body: Value) -> Value {
    let response = client
        .post(format!("{}{path}", server.url))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .expect("terminal API request");
    let status = response.status();
    let envelope: Value = response.json().await.expect("terminal API JSON");
    assert_eq!(status, StatusCode::OK, "{envelope}");
    assert_eq!(envelope["kind"], "result", "{envelope}");
    envelope["result"].clone()
}

async fn issue_ui(client: &Client, server: &TestServer) -> String {
    let response = client
        .post(format!("{}/ui-attachments", server.url))
        .bearer_auth(TOKEN)
        .json(&json!({"initial_session_id": SESSION_ID}))
        .send()
        .await
        .expect("issue terminal UI");
    let status = response.status();
    let attachment: Value = response.json().await.expect("terminal UI attachment");
    assert_eq!(status, StatusCode::OK, "{attachment}");
    attachment["ui_access_url_secret"]
        .as_str()
        .expect("AFUI access URL")
        .to_owned()
}

async fn public_screen(client: &Client, server: &TestServer) -> Value {
    let response = client
        .get(format!("{}/v1/sessions/{SESSION_ID}/screen", server.url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("read terminal screen");
    let status = response.status();
    let envelope: Value = response.json().await.expect("terminal screen JSON");
    assert_eq!(status, StatusCode::OK, "{envelope}");
    envelope["result"].clone()
}

async fn wait_for_public_marker(client: &Client, server: &TestServer, marker: &str) {
    for _attempt in 0..100 {
        let screen = public_screen(client, server).await;
        let rendered = screen["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        if rendered.contains(marker) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("terminal never displayed {marker}");
}

async fn retained_state(access_url: &str, client_id: &str) -> Value {
    let mut runtime = RuntimeClient::connect(access_url, client_id)
        .await
        .expect("connect through AFUI test support");
    let state = runtime
        .opening_state::<Value>()
        .await
        .expect("read AFUI retained state")
        .expect("terminal published opening state");
    runtime.close().await.expect("close AFUI test client");
    state
}

fn terminal_screen(state: &Value) -> Value {
    assert_eq!(state["type"], "snapshot", "{state}");
    state["sessions"]
        .as_array()
        .expect("terminal sessions")
        .iter()
        .find(|session| session["session_id"] == SESSION_ID)
        .unwrap_or_else(|| panic!("retained state omitted {SESSION_ID}: {state}"))["screen"]
        .clone()
}

fn rendered_screen(mut screen: Value) -> Value {
    if let Some(object) = screen.as_object_mut() {
        // This is a live clock, not a value the page has to reconstruct.
        object.remove("activity");
    }
    screen
}

fn screen_text(screen: &Value) -> String {
    screen["lines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_state_rebuilds_the_page_and_typed_calls_keep_secrets_out() {
    let server = TestServer::start().await;
    let client = Client::new();
    post_result(
        &client,
        &server,
        "/v1/sessions",
        json!({"session_id": SESSION_ID, "program": "/bin/sh"}),
    )
    .await;
    let access_url = issue_ui(&client, &server).await;

    let mut runtime = RuntimeClient::connect(&access_url, "terminal-action")
        .await
        .expect("connect terminal action client");
    runtime
        .opening_state::<Value>()
        .await
        .expect("terminal opening state")
        .expect("terminal retained state");
    let command = b"printf 'FIRST_LOAD\\n'\n";
    let reply: Value = runtime
        .call(&json!({
            "type": "input",
            "session_id": SESSION_ID,
            "data_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                command,
            ),
        }))
        .await
        .expect("typed terminal input");
    assert_eq!(reply["type"], "input_accepted", "{reply}");
    assert_eq!(reply["input_bytes"], command.len(), "{reply}");
    wait_for_public_marker(&client, &server, "FIRST_LOAD").await;

    let first = terminal_screen(&retained_state(&access_url, "first-load").await);
    let second = terminal_screen(&retained_state(&access_url, "second-load").await);
    assert!(screen_text(&first).contains("FIRST_LOAD"), "{first}");
    assert_eq!(rendered_screen(first), rendered_screen(second));

    let started: Value = runtime
        .call(&json!({
            "type": "secret_input",
            "session_id": SESSION_ID,
            "action": "start",
        }))
        .await
        .expect("start secret input");
    assert_eq!(
        started,
        json!({"type": "secret_input_changed", "active": true})
    );
    let secret_command = format!("printf '{SECRET}\\n'\n");
    let _accepted: Value = runtime
        .call(&json!({
            "type": "input",
            "session_id": SESSION_ID,
            "data_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                secret_command.as_bytes(),
            ),
        }))
        .await
        .expect("type through the secret window");
    let withheld = terminal_screen(&retained_state(&access_url, "during-secret").await);
    assert_eq!(withheld["secret_input"], true, "{withheld}");
    assert_eq!(withheld["lines"], json!([]), "{withheld}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result: Result<Value, _> = runtime
            .call(&json!({
                "type": "secret_input",
                "session_id": SESSION_ID,
                "action": "end",
            }))
            .await;
        if let Ok(reply) = result {
            assert_eq!(
                reply,
                json!({"type": "secret_input_changed", "active": false})
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "secret input never settled"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let after = b"printf 'AFTER_SECRET\\n'\n";
    let _accepted: Value = runtime
        .call(&json!({
            "type": "input",
            "session_id": SESSION_ID,
            "data_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                after,
            ),
        }))
        .await
        .expect("input after secret window");
    wait_for_public_marker(&client, &server, "AFTER_SECRET").await;
    let recovered = terminal_screen(&retained_state(&access_url, "after-secret").await);
    let text = screen_text(&recovered);
    assert!(text.contains("AFTER_SECRET"), "{recovered}");
    assert!(!text.contains(SECRET), "{recovered}");
    runtime.close().await.expect("close terminal action client");
}
