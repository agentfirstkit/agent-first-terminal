use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    ActivityState, CursorState, DEFAULT_INPUT_LEASE_TTL_MS, EventEnvelope, InputActor,
    InputActorKind, InputLease, InputLeaseMode, ScreenCell, ScreenColor, ScreenSnapshot,
    SecretInputStatus, TerminalEvent, TerminalModes, TerminalOpenSpec, TerminalSessionMeta,
    TerminalSessionStatus, TerminalSignal,
};

pub(crate) const DEFAULT_ROWS: u16 = 24;
pub(crate) const DEFAULT_COLS: u16 = 80;

fn default_rows() -> u16 {
    DEFAULT_ROWS
}

fn default_cols() -> u16 {
    DEFAULT_COLS
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenSessionRequest {
    pub session_id: String,
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd_path: Option<String>,
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default = "default_cols")]
    pub cols: u16,
    pub title: Option<String>,
}

impl OpenSessionRequest {
    pub(crate) fn into_spec(self) -> TerminalOpenSpec {
        TerminalOpenSpec {
            program: self.program,
            args: self.args,
            cwd: self.cwd_path.map(PathBuf::from),
            rows: self.rows,
            cols: self.cols,
            title: self.title,
            // Every session this type produces came in over HTTP.
            api_requested: true,
            ..TerminalOpenSpec::default()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendInputRequest {
    pub actor: ActorModel,
    pub lease_id: Option<String>,
    pub data_base64: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResizeRequest {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActorKindName {
    Human,
    Agent,
    Renderer,
    Controller,
    Test,
    Replay,
}

impl From<ActorKindName> for InputActorKind {
    fn from(kind: ActorKindName) -> Self {
        match kind {
            ActorKindName::Human => Self::Human,
            ActorKindName::Agent => Self::Agent,
            ActorKindName::Renderer => Self::Renderer,
            ActorKindName::Controller => Self::Controller,
            ActorKindName::Test => Self::Test,
            ActorKindName::Replay => Self::Replay,
        }
    }
}

impl From<InputActorKind> for ActorKindName {
    fn from(kind: InputActorKind) -> Self {
        match kind {
            InputActorKind::Human => Self::Human,
            InputActorKind::Agent => Self::Agent,
            InputActorKind::Renderer => Self::Renderer,
            InputActorKind::Controller => Self::Controller,
            InputActorKind::Test => Self::Test,
            InputActorKind::Replay => Self::Replay,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActorModel {
    pub kind: ActorKindName,
    pub id: String,
}

impl From<ActorModel> for InputActor {
    fn from(actor: ActorModel) -> Self {
        Self {
            kind: actor.kind.into(),
            id: actor.id,
        }
    }
}

impl From<InputActor> for ActorModel {
    fn from(actor: InputActor) -> Self {
        Self {
            kind: actor.kind.into(),
            id: actor.id,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeaseModeName {
    Shared,
    Exclusive,
}

impl From<LeaseModeName> for InputLeaseMode {
    fn from(mode: LeaseModeName) -> Self {
        match mode {
            LeaseModeName::Shared => Self::Shared,
            LeaseModeName::Exclusive => Self::Exclusive,
        }
    }
}

impl From<InputLeaseMode> for LeaseModeName {
    fn from(mode: InputLeaseMode) -> Self {
        match mode {
            InputLeaseMode::Shared => Self::Shared,
            InputLeaseMode::Exclusive => Self::Exclusive,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcquireInputLeaseRequest {
    pub actor: ActorModel,
    pub mode: LeaseModeName,
    #[serde(default = "default_input_lease_ttl_ms")]
    pub ttl_ms: u64,
    pub lease_id: Option<String>,
}

fn default_input_lease_ttl_ms() -> u64 {
    DEFAULT_INPUT_LEASE_TTL_MS
}

#[derive(Debug, Serialize)]
pub(crate) struct InputLeaseResult {
    pub lease_id: String,
    pub actor: ActorModel,
    pub mode: LeaseModeName,
    pub ttl_ms: u64,
    pub remaining_ttl_ms: u64,
}

impl From<InputLease> for InputLeaseResult {
    fn from(lease: InputLease) -> Self {
        Self {
            lease_id: lease.lease_id,
            actor: lease.actor.into(),
            mode: lease.mode.into(),
            ttl_ms: lease.ttl_ms,
            remaining_ttl_ms: lease.remaining_ttl_ms,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct InputLeaseListResult {
    pub leases: Vec<InputLeaseResult>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignalName {
    Interrupt,
    Terminate,
    Kill,
}

impl From<SignalName> for TerminalSignal {
    fn from(signal: SignalName) -> Self {
        match signal {
            SignalName::Interrupt => Self::Interrupt,
            SignalName::Terminate => Self::Terminate,
            SignalName::Kill => Self::Kill,
        }
    }
}

impl From<TerminalSignal> for SignalName {
    fn from(signal: TerminalSignal) -> Self {
        match signal {
            TerminalSignal::Interrupt => Self::Interrupt,
            TerminalSignal::Terminate => Self::Terminate,
            TerminalSignal::Kill => Self::Kill,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendSignalRequest {
    pub actor: ActorModel,
    pub lease_id: Option<String>,
    pub signal: SignalName,
}

#[derive(Debug, Serialize)]
pub(crate) struct HealthResult {
    pub service: &'static str,
    pub version: &'static str,
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct SessionListResult {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SessionInfo {
    pub session_id: String,
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub rows: u16,
    pub cols: u16,
    pub title: Option<String>,
    pub secret_input: bool,
}

impl SessionInfo {
    pub(crate) fn from_meta(session_id: String, meta: TerminalSessionMeta) -> Self {
        let (status, exit_code) = match meta.status {
            TerminalSessionStatus::Running => ("running", None),
            TerminalSessionStatus::Exited(code) => ("exited", code),
            TerminalSessionStatus::Error(_) => ("error", None),
        };
        Self {
            session_id,
            status,
            exit_code,
            rows: meta.rows,
            cols: meta.cols,
            title: meta.title,
            secret_input: meta.secret_input,
        }
    }
}

/// The two things a caller can do to a session's secret-input window, as one
/// closed tagged union.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SecretInputActionRequest {
    /// Open the window. Any actor may; a prompt detector should be able to.
    Start { actor: ActorModel, reason: String },
    /// Close it again. Only a human actor may.
    End { actor: ActorModel },
}

impl SecretInputActionRequest {
    pub(crate) fn actor(&self) -> &ActorModel {
        match self {
            Self::Start { actor, .. } | Self::End { actor } => actor,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct SecretInputResult {
    pub session_id: String,
    pub secret_input: bool,
    pub actor: Option<ActorModel>,
    pub reason: Option<String>,
}

impl SecretInputResult {
    pub(crate) fn new(session_id: String, status: SecretInputStatus) -> Self {
        Self {
            session_id,
            secret_input: status.active,
            actor: status.actor.map(Into::into),
            reason: status.reason,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct InputAck {
    pub accepted: bool,
    pub input_bytes: usize,
    pub actor: ActorModel,
}

#[derive(Debug, Serialize)]
pub(crate) struct SignalAck {
    pub delivered: bool,
    pub signal: SignalName,
    pub actor: ActorModel,
}

#[derive(Debug, Serialize)]
pub(crate) struct ScreenResult {
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub title: Option<String>,
    pub cursor: CursorResult,
    pub alt_screen: bool,
    pub lines: Vec<String>,
    pub cells: Vec<Vec<ScreenCellResult>>,
    pub modes: TerminalModesResult,
    pub unsupported_extensions: Vec<&'static str>,
    pub activity: ActivityResult,
    pub secret_input: bool,
}

impl From<ScreenSnapshot> for ScreenResult {
    fn from(snapshot: ScreenSnapshot) -> Self {
        Self {
            seq: snapshot.seq,
            cols: snapshot.cols,
            rows: snapshot.rows,
            title: snapshot.title,
            cursor: snapshot.cursor.into(),
            alt_screen: snapshot.alt_screen,
            lines: snapshot.lines,
            cells: snapshot
                .cells
                .into_iter()
                .map(|row| row.into_iter().map(Into::into).collect())
                .collect(),
            modes: snapshot.modes.into(),
            unsupported_extensions: snapshot.unsupported_extensions,
            activity: snapshot.activity.into(),
            secret_input: snapshot.secret_input,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ScreenCellResult {
    pub text: String,
    pub width: u8,
    pub foreground: ScreenColorResult,
    pub background: ScreenColorResult,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl From<ScreenCell> for ScreenCellResult {
    fn from(cell: ScreenCell) -> Self {
        Self {
            text: cell.text,
            width: cell.width,
            foreground: cell.foreground.into(),
            background: cell.background.into(),
            bold: cell.bold,
            dim: cell.dim,
            italic: cell.italic,
            underline: cell.underline,
            inverse: cell.inverse,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ScreenColorResult {
    Default,
    Indexed { index: u8 },
    Rgb { red: u8, green: u8, blue: u8 },
}

impl From<ScreenColor> for ScreenColorResult {
    fn from(color: ScreenColor) -> Self {
        match color {
            ScreenColor::Default => Self::Default,
            ScreenColor::Indexed(index) => Self::Indexed { index },
            ScreenColor::Rgb { red, green, blue } => Self::Rgb { red, green, blue },
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct TerminalModesResult {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
}

impl From<TerminalModes> for TerminalModesResult {
    fn from(modes: TerminalModes) -> Self {
        Self {
            application_cursor: modes.application_cursor,
            bracketed_paste: modes.bracketed_paste,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CursorResult {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

impl From<CursorState> for CursorResult {
    fn from(cursor: CursorState) -> Self {
        Self {
            row: cursor.row,
            col: cursor.col,
            visible: cursor.visible,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ActivityResult {
    pub last_output_age_ms: u64,
    pub quiescent: bool,
}

impl From<ActivityState> for ActivityResult {
    fn from(activity: ActivityState) -> Self {
        Self {
            last_output_age_ms: activity.last_output_age_ms,
            quiescent: activity.quiescent,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct EventEnvelopeResult {
    pub seq: u64,
    pub session_id: String,
    pub event: EventResult,
}

impl From<EventEnvelope> for EventEnvelopeResult {
    fn from(envelope: EventEnvelope) -> Self {
        Self {
            seq: envelope.seq,
            session_id: envelope.session_id,
            event: envelope.event.into(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum EventResult {
    SessionOpened,
    ScreenChanged {
        screen_seq: u64,
    },
    OutputChunk {
        chunk_bytes: usize,
    },
    Resized {
        rows: u16,
        cols: u16,
    },
    InputAccepted {
        actor: ActorModel,
        input_bytes: usize,
        lease_id: Option<String>,
    },
    InputRejected {
        actor: ActorModel,
        reason: &'static str,
    },
    InputPreempted {
        previous_actor: ActorModel,
        by_actor: ActorModel,
        lease_id: String,
    },
    InputLeaseAcquired {
        lease: InputLeaseResult,
    },
    InputLeaseReleased {
        lease_id: String,
        actor: ActorModel,
        reason: &'static str,
    },
    SignalSent {
        signal: SignalName,
        actor: Option<ActorModel>,
        lease_id: Option<String>,
    },
    SecretInputStarted {
        actor: ActorModel,
        reason: String,
    },
    SecretInputEnded {
        actor: ActorModel,
    },
    ProcessExited {
        exit_code: Option<i32>,
    },
}

impl From<TerminalEvent> for EventResult {
    fn from(event: TerminalEvent) -> Self {
        match event {
            TerminalEvent::SessionOpened => Self::SessionOpened,
            TerminalEvent::ScreenChanged { screen_seq } => Self::ScreenChanged { screen_seq },
            TerminalEvent::OutputChunk { chunk_bytes } => Self::OutputChunk { chunk_bytes },
            TerminalEvent::Resized { rows, cols } => Self::Resized { rows, cols },
            TerminalEvent::InputAccepted {
                actor,
                input_bytes,
                lease_id,
            } => Self::InputAccepted {
                actor: actor.into(),
                input_bytes,
                lease_id,
            },
            TerminalEvent::InputRejected { actor, reason } => Self::InputRejected {
                actor: actor.into(),
                reason: reason.as_word(),
            },
            TerminalEvent::InputPreempted {
                previous_actor,
                by_actor,
                lease_id,
            } => Self::InputPreempted {
                previous_actor: previous_actor.into(),
                by_actor: by_actor.into(),
                lease_id,
            },
            TerminalEvent::InputLeaseAcquired { lease } => Self::InputLeaseAcquired {
                lease: lease.into(),
            },
            TerminalEvent::InputLeaseReleased {
                lease_id,
                actor,
                reason,
            } => Self::InputLeaseReleased {
                lease_id,
                actor: actor.into(),
                reason: reason.as_word(),
            },
            TerminalEvent::SignalSent {
                signal,
                actor,
                lease_id,
            } => Self::SignalSent {
                signal: signal.into(),
                actor: actor.map(Into::into),
                lease_id,
            },
            TerminalEvent::SecretInputStarted { actor, reason } => Self::SecretInputStarted {
                actor: actor.into(),
                reason,
            },
            TerminalEvent::SecretInputEnded { actor } => Self::SecretInputEnded {
                actor: actor.into(),
            },
            TerminalEvent::ProcessExited { code } => Self::ProcessExited { exit_code: code },
        }
    }
}
