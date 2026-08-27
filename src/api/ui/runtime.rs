use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use agent_first_ui::{UiCallError, UiSessionRuntime};
use base64::Engine;
use serde::{Deserialize, Serialize};

use super::super::model::{ScreenResult, SessionInfo, SignalName};
use super::super::server::{ApiState, validate_session_id};
use crate::{InputActor, InputActorKind, TerminalError};

const MAX_UI_INPUT_BYTES: usize = 1024 * 1024;
const UI_ACTOR_ID: &str = "local-ui";

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TerminalUiAction {
    Input {
        session_id: String,
        data_base64: String,
    },
    InputAction {
        session_id: String,
        action: TerminalUiInputAction,
        ctrl: bool,
    },
    SecretInput {
        session_id: String,
        action: TerminalUiSecretInputAction,
    },
    Resize {
        session_id: String,
        rows: u16,
        cols: u16,
    },
    Signal {
        session_id: String,
        signal: SignalName,
    },
    Close {
        session_id: String,
    },
}

impl TerminalUiAction {
    fn session_id(&self) -> &str {
        match self {
            Self::Input { session_id, .. }
            | Self::InputAction { session_id, .. }
            | Self::SecretInput { session_id, .. }
            | Self::Resize { session_id, .. }
            | Self::Signal { session_id, .. }
            | Self::Close { session_id } => session_id,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalUiInputAction {
    Return,
    Backspace,
    Tab,
    Escape,
    ArrowLeft,
    ArrowDown,
    ArrowUp,
    ArrowRight,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalUiSecretInputAction {
    Start,
    End,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TerminalUiReply {
    InputAccepted { input_bytes: usize },
    SecretInputChanged { active: bool },
    Resized { rows: u16, cols: u16 },
    SignalSent { signal: SignalName },
    Closed,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TerminalUiState {
    Snapshot { sessions: Vec<TerminalUiSession> },
    Error { code: &'static str, message: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct TerminalUiSession {
    #[serde(flatten)]
    info: SessionInfo,
    screen: Option<ScreenResult>,
}

pub(super) type TerminalRuntime =
    UiSessionRuntime<TerminalUiAction, TerminalUiReply, TerminalUiState>;

/// Publish the complete state before a page can connect.
///
/// The runtime then replaces this retained value whenever the terminal event
/// bus changes. A reconnect receives the newest complete state from AFUI; no
/// surface has to reconcile an SSE gap or ask for a second snapshot.
pub(super) fn publish_opening_state(
    state: &ApiState,
    runtime: &TerminalRuntime,
) -> agent_first_ui::Result<()> {
    publish_state(state, runtime)
}

pub(super) async fn run(state: ApiState, runtime: TerminalRuntime) {
    let subscription = state
        .manager
        .lock()
        .ok()
        .map(|manager| manager.subscribe_events());
    let mut events = tokio::time::interval(Duration::from_millis(16));
    events.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut reconciliation = tokio::time::interval(Duration::from_secs(1));
    reconciliation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The opening state was published before the credential became visible.
    events.tick().await;
    reconciliation.tick().await;

    loop {
        tokio::select! {
            call = runtime.recv() => {
                let call = match call {
                    Ok(Some(call)) => call,
                    Ok(None) | Err(_) => return,
                };
                let outcome = execute_action(&state, call.action());
                match outcome {
                    Ok(reply) => {
                        if publish_state(&state, &runtime).is_err()
                            || call.finish(&reply).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(failure) => {
                        if call.fail(failure).await.is_err() {
                            return;
                        }
                    }
                }
            }
            _ = events.tick() => {
                let changed = subscription.as_ref().is_some_and(|subscription| {
                    let mut changed = false;
                    loop {
                        match subscription.receiver.try_recv() {
                            Ok(_event) => changed = true,
                            Err(TryRecvError::Empty) => return changed,
                            Err(TryRecvError::Disconnected) => return changed,
                        }
                    }
                });
                if changed && publish_state(&state, &runtime).is_err() {
                    return;
                }
            }
            _ = reconciliation.tick() => {
                // API callers can remove an already-exited session without a
                // new terminal event. This low-rate comparison point ensures
                // the retained UI projection still converges.
                if publish_state(&state, &runtime).is_err() {
                    return;
                }
            }
        }
    }
}

fn publish_state(state: &ApiState, runtime: &TerminalRuntime) -> agent_first_ui::Result<()> {
    let state = terminal_state(state);
    match runtime.publish_state(&state) {
        Ok(_) => Ok(()),
        Err(agent_first_ui::Error::SessionRuntimeMessageTooLarge { .. }) => runtime
            .publish_state(&TerminalUiState::Error {
                code: "terminal_ui_state_too_large",
                message: "The current terminal screens exceed this UI session's state limit."
                    .to_string(),
            })
            .map(|_| ()),
        Err(error) => Err(error),
    }
}

fn terminal_state(state: &ApiState) -> TerminalUiState {
    let Ok(mut manager) = state.manager.lock() else {
        return TerminalUiState::Error {
            code: "runtime_lock_poisoned",
            message: "Terminal runtime state is unavailable.".to_string(),
        };
    };
    let ids = manager.ids();
    let mut sessions = Vec::with_capacity(ids.len());
    for session_id in ids {
        let _status = manager.status(&session_id);
        let Some(meta) = manager.metadata(&session_id) else {
            continue;
        };
        sessions.push(TerminalUiSession {
            info: SessionInfo::from_meta(session_id.clone(), meta),
            screen: manager.screen(&session_id).map(ScreenResult::from),
        });
    }
    TerminalUiState::Snapshot { sessions }
}

fn execute_action(
    state: &ApiState,
    action: &TerminalUiAction,
) -> Result<TerminalUiReply, UiCallError> {
    validate_session_id(action.session_id())
        .map_err(|message| UiCallError::new("invalid_session_id", message))?;
    let mut manager = state.manager.lock().map_err(|_| {
        UiCallError::new(
            "runtime_lock_poisoned",
            "Terminal runtime state is unavailable.",
        )
    })?;
    match action {
        TerminalUiAction::Input {
            session_id,
            data_base64,
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .map_err(|error| {
                    UiCallError::new(
                        "invalid_input_base64",
                        format!("data_base64 is invalid: {error}"),
                    )
                })?;
            if bytes.len() > MAX_UI_INPUT_BYTES {
                return Err(UiCallError::new(
                    "input_too_large",
                    format!("decoded input exceeds {MAX_UI_INPUT_BYTES} bytes"),
                ));
            }
            manager
                .write_as(session_id, ui_actor(), None, &bytes)
                .map_err(terminal_call_error)?;
            Ok(TerminalUiReply::InputAccepted {
                input_bytes: bytes.len(),
            })
        }
        TerminalUiAction::InputAction {
            session_id,
            action,
            ctrl,
        } => {
            let screen = manager
                .screen(session_id)
                .ok_or_else(|| terminal_call_error(TerminalError::NotFound(session_id.clone())))?;
            let bytes = input_action_bytes(*action, *ctrl, screen.modes.application_cursor);
            manager
                .write_as(session_id, ui_actor(), None, bytes)
                .map_err(terminal_call_error)?;
            Ok(TerminalUiReply::InputAccepted {
                input_bytes: bytes.len(),
            })
        }
        TerminalUiAction::SecretInput { session_id, action } => {
            let status = match action {
                TerminalUiSecretInputAction::Start => {
                    manager.enter_secret(session_id, ui_actor(), "started from the terminal window")
                }
                TerminalUiSecretInputAction::End => manager.exit_secret(session_id, ui_actor()),
            }
            .map_err(terminal_call_error)?;
            Ok(TerminalUiReply::SecretInputChanged {
                active: status.active,
            })
        }
        TerminalUiAction::Resize {
            session_id,
            rows,
            cols,
        } => {
            manager
                .resize(session_id, *rows, *cols)
                .map_err(terminal_call_error)?;
            Ok(TerminalUiReply::Resized {
                rows: *rows,
                cols: *cols,
            })
        }
        TerminalUiAction::Signal { session_id, signal } => {
            manager
                .signal_as(session_id, ui_actor(), None, (*signal).into())
                .map_err(terminal_call_error)?;
            Ok(TerminalUiReply::SignalSent { signal: *signal })
        }
        TerminalUiAction::Close { session_id } => {
            manager.close(session_id).map_err(terminal_call_error)?;
            Ok(TerminalUiReply::Closed)
        }
    }
}

fn input_action_bytes(
    action: TerminalUiInputAction,
    ctrl: bool,
    application_cursor: bool,
) -> &'static [u8] {
    match (action, ctrl, application_cursor) {
        (TerminalUiInputAction::Return, _, _) => b"\r",
        (TerminalUiInputAction::Backspace, _, _) => b"\x7f",
        (TerminalUiInputAction::Tab, false, _) => b"\t",
        (TerminalUiInputAction::Tab, true, _) => b"\x1b[Z",
        (TerminalUiInputAction::Escape, _, _) => b"\x1b",
        (TerminalUiInputAction::ArrowLeft, true, _) => b"\x1b[1;5D",
        (TerminalUiInputAction::ArrowDown, true, _) => b"\x1b[1;5B",
        (TerminalUiInputAction::ArrowUp, true, _) => b"\x1b[1;5A",
        (TerminalUiInputAction::ArrowRight, true, _) => b"\x1b[1;5C",
        (TerminalUiInputAction::ArrowLeft, false, true) => b"\x1bOD",
        (TerminalUiInputAction::ArrowDown, false, true) => b"\x1bOB",
        (TerminalUiInputAction::ArrowUp, false, true) => b"\x1bOA",
        (TerminalUiInputAction::ArrowRight, false, true) => b"\x1bOC",
        (TerminalUiInputAction::ArrowLeft, false, false) => b"\x1b[D",
        (TerminalUiInputAction::ArrowDown, false, false) => b"\x1b[B",
        (TerminalUiInputAction::ArrowUp, false, false) => b"\x1b[A",
        (TerminalUiInputAction::ArrowRight, false, false) => b"\x1b[C",
    }
}

fn terminal_call_error(error: TerminalError) -> UiCallError {
    let (code, retryable, hint) = match &error {
        TerminalError::NotFound(_) => ("session_not_found", false, None),
        TerminalError::AlreadyOpen(_) => ("session_already_open", false, None),
        TerminalError::NotRunning(_) => ("session_not_running", false, None),
        TerminalError::UnsupportedSignal(_) => ("signal_not_supported", false, None),
        TerminalError::InputLeaseRequired { .. } => ("input_lease_required", false, None),
        TerminalError::InputLeaseNotFound { .. } => ("input_lease_not_found", false, None),
        TerminalError::InputLeaseConflict { .. } => ("input_lease_conflict", true, None),
        TerminalError::SecretInputActive { .. } => (
            "secret_input_active",
            true,
            Some("wait until the retained terminal state says secret input has ended"),
        ),
        TerminalError::SecretInputExitDenied { .. } => (
            "secret_input_exit_denied",
            false,
            Some("only a human actor ends secret input mode"),
        ),
        TerminalError::SecretInputSettling { .. } => (
            "secret_input_settling",
            true,
            Some("repeat the action once the session stops producing output"),
        ),
        TerminalError::InvalidSecretInputReason(_) => ("invalid_secret_input_reason", false, None),
        TerminalError::InvalidInputLeaseTtl { .. } => ("invalid_lease_ttl", false, None),
        TerminalError::InvalidDimensions { .. } => ("invalid_dimensions", false, None),
        TerminalError::InvalidInputActor(_) => ("invalid_actor", false, None),
        TerminalError::Poisoned => ("runtime_lock_poisoned", false, None),
        TerminalError::Io(_) => ("terminal_io_error", false, None),
        TerminalError::Backend(_) => ("terminal_backend_error", false, None),
    };
    let mut failure = UiCallError::new(code, error.to_string()).retryable(retryable);
    if let Some(hint) = hint {
        failure = failure.with_hint(hint);
    }
    failure
}

fn ui_actor() -> InputActor {
    InputActor {
        kind: InputActorKind::Human,
        id: UI_ACTOR_ID.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::{
        TerminalUiAction, TerminalUiInputAction, execute_action, input_action_bytes, terminal_state,
    };
    use crate::api::ApiState;

    #[test]
    fn actions_are_domain_payloads_without_transport_identity() {
        let action: TerminalUiAction = serde_json::from_value(serde_json::json!({
            "type": "resize",
            "session_id": "shell",
            "rows": 24,
            "cols": 80,
        }))
        .expect("typed action");
        assert!(matches!(action, TerminalUiAction::Resize { .. }));
        assert!(
            serde_json::from_value::<TerminalUiAction>(serde_json::json!({
                "type": "resize",
                "request_id": "consumer-owned",
                "session_id": "shell",
                "rows": 24,
                "cols": 80,
            }))
            .is_err()
        );
    }

    #[test]
    fn cursor_actions_follow_the_terminal_mode() {
        assert_eq!(
            input_action_bytes(TerminalUiInputAction::ArrowUp, false, false),
            b"\x1b[A"
        );
        assert_eq!(
            input_action_bytes(TerminalUiInputAction::ArrowUp, false, true),
            b"\x1bOA"
        );
        assert_eq!(
            input_action_bytes(TerminalUiInputAction::ArrowUp, true, true),
            b"\x1b[1;5A"
        );
    }

    #[test]
    fn invalid_geometry_is_a_typed_call_failure() {
        let state = ApiState::new("test-token".to_string());
        let failure = execute_action(
            &state,
            &TerminalUiAction::Resize {
                session_id: "shell".to_string(),
                rows: 1,
                cols: 80,
            },
        )
        .expect_err("one row is outside the terminal contract");
        assert_eq!(failure.code(), "invalid_dimensions");
        assert!(!failure.is_retryable());
    }

    #[test]
    fn retained_state_contains_domain_values_but_no_transport_fields() {
        let state = ApiState::new("test-token".to_string());
        let value = serde_json::to_value(terminal_state(&state)).expect("state serializes");
        assert_eq!(value["type"], "snapshot");
        assert_eq!(value["sessions"], serde_json::json!([]));
        let text = value.to_string();
        assert!(!text.contains("request_id"));
        assert!(!text.contains("revision"));
        assert!(!text.contains("sequence"));
    }

    #[cfg(unix)]
    #[test]
    fn human_ui_input_preempts_an_agent_exclusive_lease() {
        use crate::{
            InputActor, InputActorKind, InputLeaseMode, TerminalOpenSpec, TerminalSessionManager,
        };

        let session_id = "ui_human";
        let mut manager = TerminalSessionManager::new();
        manager
            .open(
                session_id.to_string(),
                TerminalOpenSpec {
                    program: Some("/bin/sh".to_string()),
                    ..TerminalOpenSpec::default()
                },
            )
            .expect("session opens");
        manager
            .acquire_lease(
                session_id,
                InputActor {
                    kind: InputActorKind::Agent,
                    id: "inspector".to_string(),
                },
                InputLeaseMode::Exclusive,
                60_000,
                None,
            )
            .expect("exclusive lease");
        let state = ApiState::with_manager(manager, "test-token".to_string());
        let reply = execute_action(
            &state,
            &TerminalUiAction::Input {
                session_id: session_id.to_string(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(b"printf ok\n"),
            },
        )
        .expect("human input is accepted");
        assert!(matches!(
            reply,
            super::TerminalUiReply::InputAccepted { .. }
        ));
        assert_eq!(
            state
                .manager
                .lock()
                .expect("manager")
                .leases(session_id)
                .expect("leases")
                .len(),
            0
        );
    }
}
