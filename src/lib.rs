//! Local trusted terminal runtime for Agent-First hosts.
//!
//! The runtime owns PTY-backed shell sessions, raw byte fan-out, resize, status,
//! and bounded scrollback. It does not know about application UI documents or
//! task completion. Hosts and controllers decide which sessions to open.
//!
//! Commands reach a session through actor-aware
//! [`TerminalSessionManager::write_as`] or the trusted host-only
//! [`TerminalSessionManager::write`] helper.
//!
//! Phase 2: The reader thread for each session maintains a live VT screen model
//! using the `vt100` crate. It broadcasts raw bytes to subscribers, keeps a
//! bounded scrollback ring, and updates a screen snapshot (text lines, cursor,
//! activity timestamp) that is accessible via the `screen()` method.
//!
//! Phase 3: A single global (multiplexed) event bus is shared by every
//! session. Session lifecycle, output, resize, signal, and exit are announced as
//! `session_id`-tagged [`EventEnvelope`]s with a monotonic global `seq`, so a
//! UI or orchestrator can watch many sessions on one stream and tell which one
//! changed. Event payloads never carry raw bytes; the byte stream stays on
//! `subscribe()`.

#[cfg(feature = "api")]
pub mod api;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Serializes PTY allocation across the whole process.
///
/// Allocating a pty is not concurrency-safe on the platforms this runs on:
/// with 24 threads opening a session at the same instant, 1–5 of them fail
/// with `openpty: Os { code: -6 }`, and the suite reproduced that in 7 of 8
/// runs. The names a pty allocation hands out are process-global, so two
/// simultaneous allocations can race over the same slot; holding this lock for
/// the duration of the call is what makes "many agents open a terminal at
/// once" reliable. It is held only across the allocation, never across the
/// spawn or any I/O, so sessions still run fully in parallel once open.
static PTY_ALLOCATION: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn allocate_pty(size: PtySize) -> Result<portable_pty::PtyPair, TerminalError> {
    let _serialized = PTY_ALLOCATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    native_pty_system()
        .openpty(size)
        .map_err(|error| TerminalError::Backend(error.to_string()))
}

/// Default scrollback retained per session so a late/reconnecting subscriber can
/// be replayed the recent screen state. Bounded to keep memory flat.
const DEFAULT_RING_CAPACITY: usize = 256 * 1024;

/// The env var a PTY child sees when [`TerminalSessionManager::with_afui_delivery`]
/// has been configured.
///
/// A literal, not a dependency on the optional `agent-first-ui` crate (whose
/// own `DELIVERY_ENV` names the same variable): this module has to compile
/// without that dependency, since it also backs the plain `TerminalSessionManager`
/// API used with no UI at all. The three words this variable can hold are the
/// caller's contract to keep in sync with `agent_first_ui::UiDeliveryMode`, not
/// something this module parses or validates.
const AFUI_DELIVERY_ENV: &str = "AFUI_DELIVERY";

/// Smallest geometry the VT screen model can safely represent.
///
/// A one-row grid makes `vt100` underflow while wrapping output after a resize,
/// which kills the reader thread and leaves browser subscribers reconnecting
/// forever. Two columns also leave room for a double-width terminal cell.
pub const MIN_TERMINAL_DIMENSION: u16 = 2;

/// Largest geometry accepted by the runtime and its HTTP boundary.
pub const MAX_TERMINAL_DIMENSION: u16 = 1000;

/// Default duration granted to a new or renewed input lease.
pub const DEFAULT_INPUT_LEASE_TTL_MS: u64 = 5_000;

/// Longest input lease accepted by the runtime.
pub const MAX_INPUT_LEASE_TTL_MS: u64 = 300_000;

/// Session identity. In v1 this equals the `terminal` view id (one terminal session per
/// declared terminal view); no random source is used.
pub type TerminalSessionId = String;

/// Errors from the terminal capability.
#[derive(Debug)]
pub enum TerminalError {
    /// No session with the given id is open.
    NotFound(TerminalSessionId),
    /// A session with the given id is already open.
    AlreadyOpen(TerminalSessionId),
    /// The session exists, but its process has already exited.
    NotRunning(TerminalSessionId),
    /// The requested signal has no native implementation on this platform.
    UnsupportedSignal(TerminalSignal),
    /// A non-human actor attempted input without an active lease.
    InputLeaseRequired {
        session_id: TerminalSessionId,
        actor: InputActor,
    },
    /// A non-human actor acted on a session while a person was entering a
    /// secret into it.
    SecretInputActive {
        session_id: TerminalSessionId,
        actor: InputActor,
    },
    /// A non-human actor tried to end secret input mode. Only a person can.
    SecretInputExitDenied {
        session_id: TerminalSessionId,
        actor: InputActor,
    },
    /// Secret input mode cannot end yet: the session is still producing output,
    /// which during a window is the echo of what was just typed.
    SecretInputSettling {
        session_id: TerminalSessionId,
        quiet_for_ms: u64,
    },
    /// The supplied secret-input reason is not usable.
    InvalidSecretInputReason(String),
    /// The requested lease does not exist or has expired.
    InputLeaseNotFound {
        session_id: TerminalSessionId,
        lease_id: String,
    },
    /// A lease belongs to another actor or conflicts with an active lease.
    InputLeaseConflict {
        session_id: TerminalSessionId,
        actor: InputActor,
        held_by: Option<InputActor>,
    },
    /// The requested lease duration is outside the supported range.
    InvalidInputLeaseTtl { ttl_ms: u64 },
    /// The requested terminal geometry cannot be represented safely.
    InvalidDimensions { rows: u16, cols: u16 },
    /// An actor identifier is not valid for the runtime.
    InvalidInputActor(String),
    /// The session's internal lock was poisoned by a panicking thread.
    Poisoned,
    /// I/O against the terminal session (write/flush) failed.
    Io(std::io::Error),
    /// The underlying terminal backend failed (open/spawn/resize/clone).
    Backend(String),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalError::NotFound(id) => write!(f, "terminal session `{id}` not found"),
            TerminalError::AlreadyOpen(id) => write!(f, "terminal session `{id}` already open"),
            TerminalError::NotRunning(id) => {
                write!(f, "terminal session `{id}` is not running")
            }
            TerminalError::UnsupportedSignal(signal) => {
                write!(
                    f,
                    "terminal signal `{signal}` is not supported on this platform"
                )
            }
            TerminalError::InputLeaseRequired { session_id, actor } => write!(
                f,
                "input actor `{actor}` requires a lease for terminal session `{session_id}`"
            ),
            TerminalError::InputLeaseNotFound {
                session_id,
                lease_id,
            } => write!(
                f,
                "input lease `{lease_id}` was not found for terminal session `{session_id}`"
            ),
            TerminalError::InputLeaseConflict {
                session_id,
                actor,
                held_by,
            } => {
                write!(
                    f,
                    "input actor `{actor}` conflicts with the active lease for terminal session `{session_id}`"
                )?;
                if let Some(holder) = held_by {
                    write!(f, " held by `{holder}`")?;
                }
                Ok(())
            }
            TerminalError::SecretInputActive { session_id, actor } => write!(
                f,
                "terminal session `{session_id}` is in secret input mode; actor `{actor}` is suspended"
            ),
            TerminalError::SecretInputExitDenied { session_id, actor } => write!(
                f,
                "actor `{actor}` may not end secret input mode on terminal session `{session_id}`; only a human actor can"
            ),
            TerminalError::SecretInputSettling {
                session_id,
                quiet_for_ms,
            } => write!(
                f,
                "terminal session `{session_id}` is still producing output ({quiet_for_ms}ms quiet of {SECRET_INPUT_SETTLE_MS}ms); secret input mode cannot end yet"
            ),
            TerminalError::InvalidSecretInputReason(message) => {
                write!(f, "invalid secret input reason: {message}")
            }
            TerminalError::InvalidInputLeaseTtl { ttl_ms } => write!(
                f,
                "input lease ttl_ms `{ttl_ms}` must be between 1 and {MAX_INPUT_LEASE_TTL_MS}"
            ),
            TerminalError::InvalidDimensions { rows, cols } => write!(
                f,
                "terminal rows `{rows}` and cols `{cols}` must each be between \
                 {MIN_TERMINAL_DIMENSION} and {MAX_TERMINAL_DIMENSION}"
            ),
            TerminalError::InvalidInputActor(message) => {
                write!(f, "invalid input actor: {message}")
            }
            TerminalError::Poisoned => write!(f, "terminal session lock poisoned"),
            TerminalError::Io(error) => write!(f, "terminal io error: {error}"),
            TerminalError::Backend(error) => write!(f, "terminal backend error: {error}"),
        }
    }
}

impl std::error::Error for TerminalError {}

impl From<std::io::Error> for TerminalError {
    fn from(error: std::io::Error) -> Self {
        TerminalError::Io(error)
    }
}

/// Lifecycle status of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalSessionStatus {
    /// Shell process is running.
    Running,
    /// Shell process exited (with an exit code when known).
    Exited(Option<i32>),
    /// The session failed (reader/spawn error); message is advisory.
    #[allow(dead_code)]
    Error(String),
}

/// A process signal that can be delivered to a terminal's foreground job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSignal {
    /// Interrupt the foreground process group (`SIGINT` on Unix), equivalent
    /// to the signal generated by Ctrl-C.
    Interrupt,
    /// Ask the foreground process group to terminate (`SIGTERM` on Unix).
    Terminate,
    /// Force the foreground process group to exit (`SIGKILL` on Unix).
    Kill,
}

impl TerminalSignal {
    /// Stable lowercase API/event spelling.
    pub fn as_word(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Terminate => "terminate",
            Self::Kill => "kill",
        }
    }

    #[cfg(unix)]
    fn unix_number(self) -> libc::c_int {
        match self {
            Self::Interrupt => libc::SIGINT,
            Self::Terminate => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }
}

impl fmt::Display for TerminalSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_word())
    }
}

/// Kind of participant that can act on a terminal input stream.
/// Environment variables that carry this program's own API credential.
///
/// Removed from every PTY command. A caller that deliberately wants one inside
/// a session can still pass it through `spec.env`, which is applied after this
/// and is an explicit act rather than an inheritance nobody chose.
const API_CREDENTIAL_ENV: &[&str] = &[
    "AFTERMINAL_API_ACCESS_TOKEN_SECRET",
    "AFTERMINAL_API_ACCESS_TOKEN",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputActorKind {
    Human,
    Agent,
    Renderer,
    Controller,
    Test,
    Replay,
}

impl InputActorKind {
    pub fn as_word(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::Renderer => "renderer",
            Self::Controller => "controller",
            Self::Test => "test",
            Self::Replay => "replay",
        }
    }

    fn is_human(self) -> bool {
        matches!(self, Self::Human)
    }
}

/// Stable identity of one human or automated terminal participant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputActor {
    pub kind: InputActorKind,
    pub id: String,
}

impl fmt::Display for InputActor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.as_word(), self.id)
    }
}

/// How an actor shares terminal input ownership with other leased actors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLeaseMode {
    /// Multiple actors with shared leases may submit atomic input chunks.
    Shared,
    /// Only the holder may submit non-human input until release or expiry.
    Exclusive,
}

impl InputLeaseMode {
    pub fn as_word(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
        }
    }
}

/// Public snapshot of an active input lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputLease {
    pub lease_id: String,
    pub actor: InputActor,
    pub mode: InputLeaseMode,
    pub ttl_ms: u64,
    pub remaining_ttl_ms: u64,
}

/// Stable reason attached to a rejected actor input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRejectionReason {
    LeaseRequired,
    LeaseNotFound,
    LeaseConflict,
    SecretInputActive,
}

impl InputRejectionReason {
    pub fn as_word(self) -> &'static str {
        match self {
            Self::LeaseRequired => "lease_required",
            Self::LeaseNotFound => "lease_not_found",
            Self::LeaseConflict => "lease_conflict",
            Self::SecretInputActive => "secret_input_active",
        }
    }
}

/// Longest secret-input reason the runtime records and republishes.
pub const MAX_SECRET_INPUT_REASON_LEN: usize = 256;

/// How long a session must have produced nothing before secret input mode can
/// end.
///
/// Ending the window resumes publication, so anything the reader thread has
/// not drained yet would go out — and during a secret window that tail is the
/// echo of what was just typed. The runtime cannot label those bytes, but it
/// can decline to reopen the tap while they may still be coming: the reader
/// sits blocked in `read`, so a session that has produced nothing for this
/// long has nothing left in flight.
pub const SECRET_INPUT_SETTLE_MS: u64 = 150;

/// Whether a session is currently taking secret input, and who opened the
/// window.
///
/// The reason is operator-supplied context ("password prompt", "seed phrase")
/// that goes out on the event stream so both the person and the agent can see
/// *why* the session went quiet. It never carries what was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretInputStatus {
    pub active: bool,
    pub actor: Option<InputActor>,
    pub reason: Option<String>,
}

impl SecretInputStatus {
    fn inactive() -> Self {
        Self {
            active: false,
            actor: None,
            reason: None,
        }
    }
}

/// One open secret-input window on a session.
///
/// The screen facts captured here are the ones observers keep seeing while the
/// window is open: reporting the live values would turn the screen snapshot
/// into a keystroke counter for the secret being typed.
#[derive(Debug, Clone)]
struct SecretWindow {
    actor: InputActor,
    reason: String,
    since: Instant,
    screen_seq: u64,
    alt_screen: bool,
}

/// Why an input lease stopped being active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLeaseReleaseReason {
    Released,
    Expired,
    HumanPreempted,
}

impl InputLeaseReleaseReason {
    pub fn as_word(self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Expired => "expired",
            Self::HumanPreempted => "human_preempted",
        }
    }
}

impl TerminalSessionStatus {
    /// Stable lowercase word for the host overlay metadata.
    pub fn as_word(&self) -> &'static str {
        match self {
            TerminalSessionStatus::Running => "running",
            TerminalSessionStatus::Exited(_) => "exited",
            TerminalSessionStatus::Error(_) => "error",
        }
    }
}

/// How to start a session. This is trusted host/agent input.
#[derive(Debug, Clone)]
pub struct TerminalOpenSpec {
    /// Program to execute. `None` resolves to the platform's default shell
    /// (see [`default_shell`]).
    pub program: Option<String>,
    /// Arguments passed directly to the program.
    pub args: Vec<String>,
    /// Working directory. `None` inherits the host process cwd.
    pub cwd: Option<PathBuf>,
    /// Environment overrides applied after terminal defaults.
    pub env: BTreeMap<String, String>,
    /// Initial terminal rows.
    pub rows: u16,
    /// Initial terminal columns.
    pub cols: u16,
    /// Advisory title for the overlay metadata.
    pub title: Option<String>,
    /// Whether this session was requested over the HTTP API.
    ///
    /// The API's own bearer must not end up inside a terminal the API started:
    /// that credential can open sessions naming any program, so a shell able to
    /// read it out of its environment can drive every session on the host. A
    /// session the operator starts locally inherits the operator's environment,
    /// which is the behaviour `open_inherits_parent_process_env_vars` pins and
    /// which nested `afterminal ui` relies on.
    pub api_requested: bool,
}

impl Default for TerminalOpenSpec {
    fn default() -> Self {
        Self {
            program: None,
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            rows: 24,
            cols: 80,
            title: None,
            api_requested: false,
        }
    }
}

impl TerminalOpenSpec {
    fn resolved_program(&self) -> String {
        if let Some(program) = &self.program {
            return program.clone();
        }
        default_shell()
    }
}

/// The shell a session starts when the caller names no program.
///
/// Windows deliberately does not consult `SHELL`. It is unset for native
/// processes, and the one common way it *is* set — a session started from an
/// MSYS shell such as Git Bash — sets it to a path in MSYS's own filesystem
/// namespace (`/usr/bin/bash`), which `CreateProcessW` cannot resolve. Honouring
/// it would turn a working default into "the system cannot find the path
/// specified" for precisely the users who have it set. `COMSPEC` is the
/// variable Windows itself defines for this question.
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

/// VT screen cursor position and visibility.
#[derive(Debug, Clone)]
pub struct CursorState {
    /// Row (0-based) on the current visible screen.
    pub row: u16,
    /// Column (0-based) on the current visible screen.
    pub col: u16,
    /// Whether the cursor is visible.
    pub visible: bool,
}

/// Terminal activity metrics: recency of output and quiescence state.
#[derive(Debug, Clone)]
pub struct ActivityState {
    /// Milliseconds elapsed since the last byte was output by the PTY.
    pub last_output_age_ms: u64,
    /// True if no output has occurred for >500ms (quiescent threshold).
    pub quiescent: bool,
}

/// A color attached to a rendered terminal cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenColor {
    /// Use the UI's terminal default.
    Default,
    /// One of the terminal's 256 indexed colors.
    Indexed(u8),
    /// An exact sRGB color.
    Rgb { red: u8, green: u8, blue: u8 },
}

/// One display cell in the authoritative server-side terminal screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCell {
    /// The base character and any combining characters.
    pub text: String,
    /// Display columns occupied by this cell. Wide continuations are omitted.
    pub width: u8,
    pub foreground: ScreenColor,
    pub background: ScreenColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// Input-relevant terminal modes, interpreted once by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalModes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
}

/// A snapshot of the current VT screen state. Includes text lines, cursor
/// position, dimensions, and activity metrics. This is the primary interface
/// for reading the terminal's display state in Phase 2.
#[derive(Debug, Clone)]
pub struct ScreenSnapshot {
    /// Monotonically increasing sequence number; used by clients to detect
    /// updates and for SSE Last-Event-ID continuation.
    pub seq: u64,
    /// Current terminal width in columns.
    pub cols: u16,
    /// Current terminal height in rows.
    pub rows: u16,
    /// The terminal title, if set; otherwise the spec title or None.
    pub title: Option<String>,
    /// Cursor state on the current screen.
    pub cursor: CursorState,
    /// True if the alternate screen (e.g., vim full-screen mode) is active.
    pub alt_screen: bool,
    /// Text content of each visible screen row. Each line is trimmed of
    /// trailing whitespace. Agents can keep using this compact text view while
    /// the stock UI renders `cells` from the same authoritative parser.
    ///
    /// Empty while `secret_input` is true: the runtime keeps parsing the real
    /// screen, but withholds it for as long as a person is entering a secret.
    pub lines: Vec<String>,
    /// Styled visible rows for renderers. Empty while secret input is active.
    pub cells: Vec<Vec<ScreenCell>>,
    /// Modes needed to encode semantic keyboard actions.
    pub modes: TerminalModes,
    /// VT features the current screen engine does not represent.
    pub unsupported_extensions: Vec<&'static str>,
    /// Activity metrics.
    pub activity: ActivityState,
    /// True while the session is in secret input mode. The rest of this
    /// snapshot then describes the session as it was when that window opened,
    /// with no screen content at all.
    pub secret_input: bool,
}

/// Host-local session metadata for the snapshot overlay. Only describes the
/// session — never its scrollback (which stays in the host byte channel).
#[derive(Debug, Clone)]
pub struct TerminalSessionMeta {
    pub status: TerminalSessionStatus,
    pub rows: u16,
    pub cols: u16,
    pub title: Option<String>,
    /// True while the session is in secret input mode.
    pub secret_input: bool,
}

/// A live subscription to a session's byte stream: a snapshot of current
/// scrollback plus a receiver of subsequent raw chunks.
pub struct TerminalSubscription {
    /// Recent scrollback (raw bytes) at subscribe time, for replay.
    pub backlog: Vec<u8>,
    /// Subsequent raw byte chunks. Dropped when this subscription is dropped.
    pub receiver: Receiver<Vec<u8>>,
}

/// Lifecycle, actor/lease, output, resize, signal, and exit facts for a
/// session. Payloads never carry raw terminal or input bytes — only safe
/// metadata. The raw byte stream itself is `subscribe()`; the screen content
/// itself is `screen()`.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// A new session was opened and is ready.
    SessionOpened,
    /// The screen model advanced to a new sequence after processing output.
    ScreenChanged { screen_seq: u64 },
    /// The PTY produced `chunk_bytes` bytes of output. Never includes the bytes.
    OutputChunk { chunk_bytes: usize },
    /// The session's terminal window was resized.
    Resized { rows: u16, cols: u16 },
    /// One actor's complete input chunk was written atomically to the PTY.
    InputAccepted {
        actor: InputActor,
        input_bytes: usize,
        lease_id: Option<String>,
    },
    /// Actor input was rejected before any bytes reached the PTY.
    InputRejected {
        actor: InputActor,
        reason: InputRejectionReason,
    },
    /// Human activity revoked a non-human lease before taking input control.
    InputPreempted {
        previous_actor: InputActor,
        by_actor: InputActor,
        lease_id: String,
    },
    /// A shared or exclusive input lease was created or renewed.
    InputLeaseAcquired { lease: InputLease },
    /// An input lease was released, expired, or preempted.
    InputLeaseReleased {
        lease_id: String,
        actor: InputActor,
        reason: InputLeaseReleaseReason,
    },
    /// The operating system accepted a signal for the foreground process
    /// group. This does not assert that the process handled it or exited.
    SignalSent {
        signal: TerminalSignal,
        actor: Option<InputActor>,
        lease_id: Option<String>,
    },
    /// A person started entering a secret into this session. From here until
    /// [`TerminalEvent::SecretInputEnded`] the session publishes no output,
    /// no screen content, and no input volume — the gap is deliberate, and
    /// these two events are what say so.
    SecretInputStarted { actor: InputActor, reason: String },
    /// Secret input mode ended and the session resumed publishing.
    SecretInputEnded { actor: InputActor },
    /// The session's process exited. `code` is `None` when not yet reconciled
    /// (e.g. at EOF/kill time, before a `try_wait` observes the real code).
    ProcessExited { code: Option<i32> },
}

/// One event on the global (multiplexed) event stream: a [`TerminalEvent`]
/// tagged with the session it came from and a globally monotonic `seq`.
///
/// `seq` is monotonic **across all sessions** (not per-session), so a single
/// subscriber can order every event site-wide and, later, resume a stream via
/// SSE `Last-Event-ID`.
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub seq: u64,
    pub session_id: TerminalSessionId,
    pub event: TerminalEvent,
}

/// Bounded backlog capacity for the global event bus: enough that a subscriber
/// joining slightly late still gets recent history, small enough to keep
/// memory flat regardless of how long sessions have been running.
const EVENT_BACKLOG_CAPACITY: usize = 512;

/// The global (multiplexed) event bus. Exactly one instance lives on the
/// `TerminalSessionManager`; every session holds a clone of the same `Arc` so
/// its reader thread can emit tagged envelopes directly, without routing
/// through the manager.
///
/// Lock ordering: this is the *only* other mutex besides each session's
/// `Shared`. Code that already holds a `Shared` lock may then lock this bus
/// (`Shared` -> `EventBus`); code with no `Shared` lock held (manager-only
/// emissions, `subscribe_events`) locks this bus alone. Never lock a `Shared`
/// while holding this bus's lock.
#[derive(Default)]
struct EventBus {
    seq: u64,
    subscribers: Vec<Sender<EventEnvelope>>,
    backlog: Vec<EventEnvelope>,
}

impl EventBus {
    /// Bump the global seq, build the envelope, retain it in the bounded
    /// backlog, and fan out to subscribers — dropping any whose receiver has
    /// gone away (same `retain(|tx| tx.send(...).is_ok())` pattern as the raw
    /// byte fan-out in `Shared::push_chunk`).
    fn emit(&mut self, session_id: TerminalSessionId, event: TerminalEvent) {
        self.seq += 1;
        let envelope = EventEnvelope {
            seq: self.seq,
            session_id,
            event,
        };
        self.backlog.push(envelope.clone());
        if self.backlog.len() > EVENT_BACKLOG_CAPACITY {
            let excess = self.backlog.len() - EVENT_BACKLOG_CAPACITY;
            self.backlog.drain(0..excess);
        }
        self.subscribers
            .retain(|tx| tx.send(envelope.clone()).is_ok());
    }
}

/// A live subscription to the global (multiplexed) event stream: a snapshot of
/// recent events across every session at subscribe time, plus a receiver of
/// subsequent envelopes. Per-session filtering (matching on `session_id`) is
/// the caller's job — there is no per-session subscribe in this phase.
pub struct EventSubscription {
    /// Recent events (any session) at subscribe time, for replay.
    pub backlog: Vec<EventEnvelope>,
    /// Subsequent events, all sessions, tagged by `session_id`.
    pub receiver: Receiver<EventEnvelope>,
}

/// Answers the questions a program asks of the terminal it is running in.
///
/// A terminal is not only a sink for bytes: a program can ask it where the
/// cursor is (`ESC[6n`) or whether it is healthy (`ESC[5n`) and then *wait* for
/// the answer. Windows makes this unavoidable — ConPTY opens by asking for the
/// cursor position and does not emit so much as a shell prompt until it is
/// answered — so a terminal that stays silent here is not a degraded terminal
/// but a dead one. On Unix the same query comes from shells measuring their own
/// prompt and from full-screen programs finding their footing.
///
/// The replies are collected rather than written from inside the callback: the
/// parser runs under the `Shared` lock on the reader thread, and the writer is
/// the manager's. Handing them back lets the reader send them with no lock
/// held.
#[derive(Default)]
struct TerminalQueries {
    replies: Vec<Vec<u8>>,
}

impl vt100::Callbacks for TerminalQueries {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        _intermediate2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // Device Status Report. A private form (`ESC[?...n`) asks about
        // extensions this terminal does not implement, and is left unanswered
        // rather than answered wrongly.
        if c != 'n' || intermediate.is_some() {
            return;
        }
        match params.first().and_then(|param| param.first()).copied() {
            // "Are you there?" — report no malfunction.
            Some(5) => self.replies.push(b"\x1b[0n".to_vec()),
            // "Where is the cursor?" — CPR is 1-based, the model is 0-based.
            Some(6) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            }
            _ => {}
        }
    }
}

/// State shared between a session handle and its reader thread.
struct Shared {
    ring: Vec<u8>,
    ring_capacity: usize,
    subscribers: Vec<Sender<Vec<u8>>>,
    status: TerminalSessionStatus,
    // Phase 2: VT screen model using vt100
    parser: vt100::Parser<TerminalQueries>,
    screen_seq: u64,
    last_output: Instant,
    spec_title: Option<String>,
    rows: u16,
    cols: u16,
    // Phase 3: this session's own id and a handle to the global event bus, so
    // the reader thread can emit tagged envelopes without going through the
    // manager.
    id: TerminalSessionId,
    events: Arc<Mutex<EventBus>>,
    // Phase 6: set while a person is entering a secret into this session.
    // The reader thread reads it on every chunk, which is why it lives here
    // rather than beside the session's other manager-side state.
    secret: Option<SecretWindow>,
}

impl Shared {
    /// Feeds one chunk of output through the terminal model, and returns
    /// anything the program on the far end is waiting to be told in reply.
    #[must_use]
    fn push_chunk(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        // Phase 2: feed bytes to the VT parser and update screen state. This
        // happens first and unconditionally: the runtime's own model of the
        // terminal has to stay in step with the real one even while none of it
        // may leave the process, or the screen would be permanently wrong
        // after every secret window.
        self.parser.process(chunk);
        // Taken before the secret-mode return below: a query answered late is
        // a session that never starts, and the answer is terminal protocol
        // rather than anything derived from what is being typed.
        let replies = std::mem::take(&mut self.parser.callbacks_mut().replies);
        self.screen_seq += 1;
        self.last_output = Instant::now();

        // Phase 6: in secret input mode this is where output stops. Nothing is
        // fanned out, nothing is retained for replay, and no event describes
        // it — not even its size, which on an echoed prompt is the length of
        // what is being typed.
        if self.secret.is_some() {
            return replies;
        }

        // Push to raw byte ring for raw subscribers.
        self.ring.extend_from_slice(chunk);
        if self.ring.len() > self.ring_capacity {
            let excess = self.ring.len() - self.ring_capacity;
            self.ring.drain(0..excess);
        }
        // Drop subscribers whose receiver has gone away.
        self.subscribers
            .retain(|tx| tx.send(chunk.to_vec()).is_ok());

        // Phase 3: emit tagged events on the global bus. The caller already
        // holds this session's `Shared` lock (it called `push_chunk` through
        // it), so locking `events` here follows the required
        // `Shared` -> `EventBus` order.
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(
                self.id.clone(),
                TerminalEvent::OutputChunk {
                    chunk_bytes: chunk.len(),
                },
            );
            bus.emit(
                self.id.clone(),
                TerminalEvent::ScreenChanged {
                    screen_seq: self.screen_seq,
                },
            );
        }

        replies
    }

    /// Extract the current screen snapshot.
    ///
    /// In secret input mode this returns the withheld form: the geometry and
    /// the title (configuration, not content), no lines, no cursor, and the
    /// screen facts frozen at the moment the window opened. A caller can still
    /// see *that* the session is taking a secret — that is the one thing it
    /// must be able to see.
    fn snapshot(&self) -> ScreenSnapshot {
        if let Some(secret) = &self.secret {
            let elapsed_ms = secret.since.elapsed().as_millis() as u64;
            return ScreenSnapshot {
                seq: secret.screen_seq,
                rows: self.rows,
                cols: self.cols,
                title: self.spec_title.clone(),
                cursor: CursorState {
                    row: 0,
                    col: 0,
                    visible: false,
                },
                alt_screen: secret.alt_screen,
                lines: Vec::new(),
                cells: Vec::new(),
                modes: TerminalModes {
                    application_cursor: false,
                    bracketed_paste: false,
                },
                unsupported_extensions: vec![
                    "osc8_links",
                    "hidden",
                    "strikethrough",
                    "structured_scrollback",
                ],
                activity: ActivityState {
                    last_output_age_ms: elapsed_ms,
                    quiescent: elapsed_ms > 500,
                },
                secret_input: true,
            };
        }
        let screen = self.parser.screen();

        // Get the full screen contents and split into lines.
        let contents = screen.contents();
        let mut lines: Vec<String> = contents
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect();

        // Ensure we have at least one line (even if empty).
        if lines.is_empty() {
            lines.push(String::new());
        }

        let (cursor_row, cursor_col) = screen.cursor_position();
        let cursor_state = CursorState {
            row: cursor_row,
            col: cursor_col,
            visible: !screen.hide_cursor(),
        };

        let elapsed_ms = self.last_output.elapsed().as_millis() as u64;
        let activity = ActivityState {
            last_output_age_ms: elapsed_ms,
            quiescent: elapsed_ms > 500,
        };

        let title = self.spec_title.clone();
        let cells = (0..self.rows)
            .map(|row| {
                (0..self.cols)
                    .filter_map(|col| screen.cell(row, col))
                    .filter(|cell| !cell.is_wide_continuation())
                    .map(|cell| ScreenCell {
                        text: if cell.has_contents() {
                            cell.contents().to_string()
                        } else {
                            " ".to_string()
                        },
                        width: if cell.is_wide() { 2 } else { 1 },
                        foreground: screen_color(cell.fgcolor()),
                        background: screen_color(cell.bgcolor()),
                        bold: cell.bold(),
                        dim: cell.dim(),
                        italic: cell.italic(),
                        underline: cell.underline(),
                        inverse: cell.inverse(),
                    })
                    .collect()
            })
            .collect();

        ScreenSnapshot {
            seq: self.screen_seq,
            rows: self.rows,
            cols: self.cols,
            title,
            cursor: cursor_state,
            alt_screen: screen.alternate_screen(),
            lines,
            cells,
            modes: TerminalModes {
                application_cursor: screen.application_cursor(),
                bracketed_paste: screen.bracketed_paste(),
            },
            unsupported_extensions: vec![
                "osc8_links",
                "hidden",
                "strikethrough",
                "structured_scrollback",
            ],
            activity,
            secret_input: false,
        }
    }

    fn secret_status(&self) -> SecretInputStatus {
        match &self.secret {
            Some(secret) => SecretInputStatus {
                active: true,
                actor: Some(secret.actor.clone()),
                reason: Some(secret.reason.clone()),
            },
            None => SecretInputStatus::inactive(),
        }
    }
}

fn screen_color(color: vt100::Color) -> ScreenColor {
    match color {
        vt100::Color::Default => ScreenColor::Default,
        vt100::Color::Idx(index) => ScreenColor::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => ScreenColor::Rgb { red, green, blue },
    }
}

#[derive(Debug, Clone)]
struct ActiveInputLease {
    lease_id: String,
    actor: InputActor,
    mode: InputLeaseMode,
    ttl_ms: u64,
    expires_at: Instant,
}

struct ActorAuthorization {
    lease_id: Option<String>,
    preempted: Vec<ActiveInputLease>,
    /// True when this write lands inside a secret-input window, so the caller
    /// knows to keep its volume off the event stream.
    secret_active: bool,
}

impl ActiveInputLease {
    fn snapshot(&self, now: Instant) -> InputLease {
        let remaining_ttl_ms = self.expires_at.saturating_duration_since(now).as_millis() as u64;
        InputLease {
            lease_id: self.lease_id.clone(),
            actor: self.actor.clone(),
            mode: self.mode,
            ttl_ms: self.ttl_ms,
            remaining_ttl_ms,
        }
    }
}

/// One backend-backed shell session.
struct TerminalSession {
    master: Box<dyn MasterPty + Send>,
    /// Shared with the reader thread, which answers terminal queries on this
    /// same channel. The mutex is what keeps a reply from landing in the middle
    /// of an actor's write.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    #[allow(dead_code)]
    child: Box<dyn Child + Send + Sync>,
    shared: Arc<Mutex<Shared>>,
    rows: u16,
    cols: u16,
    title: Option<String>,
    input_leases: BTreeMap<String, ActiveInputLease>,
    // Kept so the reader thread is owned by the session; detached on drop.
    _reader: JoinHandle<()>,
}

/// Owns all live terminal sessions for a host. Hosts wrap this in
/// `Arc<Mutex<…>>`; every method goes through that lock, so `&self` (subscribe,
/// status, metadata) and `&mut self` (open, actor input, lease, resize, signal,
/// kill) are both fine.
#[derive(Default)]
pub struct TerminalSessionManager {
    sessions: HashMap<TerminalSessionId, TerminalSession>,
    next_lease_seq: u64,
    // Phase 3: the global (multiplexed) event bus, shared into every session's
    // `Shared` via `Arc::clone` so reader threads can emit directly.
    events: Arc<Mutex<EventBus>>,
    // `AFUI_DELIVERY` value handed to every PTY child this manager opens, or
    // `None` (the default) to not touch that variable in a child's
    // environment at all. See `with_afui_delivery`.
    afui_delivery: Option<String>,
}

impl Drop for TerminalSessionManager {
    fn drop(&mut self) {
        for session in self.sessions.values_mut() {
            let process_exited = session.child.try_wait().ok().flatten().is_some();
            if !process_exited {
                // Drop cannot report cleanup failures. Closing the PTY fields
                // immediately afterwards remains the backend fallback.
                let _cleanup_result = session.child.kill();
            }
        }
    }
}

impl TerminalSessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare the `AFUI_DELIVERY` value every session this manager opens
    /// should hand its PTY child.
    ///
    /// This is the one hop in the chain that has the information: a session
    /// opened here may itself run a command with a UI of its own (a mail
    /// review, a database inspector, …), and only afterminal knows whether
    /// *this* terminal is reaching a person sitting at this machine or not.
    /// A sub-command that reads `AFUI_DELIVERY` (unset counts as "window")
    /// then opens its own UI without guessing.
    ///
    /// Set once for the whole manager, not per session — it is a property of
    /// how this one `afterminal` invocation reaches a person, the same for
    /// every terminal it opens. Not calling this (the default from
    /// [`Self::new`]) leaves `AFUI_DELIVERY` untouched in a child's
    /// environment, which is a deliberate choice as much as any value: a
    /// child inherits the parent process's environment by default (see
    /// `open`), so forcing e.g. `window` here would stomp a value a person
    /// already exported in their own shell.
    #[must_use]
    pub fn with_afui_delivery(mut self, value: impl Into<String>) -> Self {
        self.afui_delivery = Some(value.into());
        self
    }

    /// Open a session under `id`. `id` is the terminal view id in v1.
    pub fn open(
        &mut self,
        id: impl Into<TerminalSessionId>,
        spec: TerminalOpenSpec,
    ) -> Result<TerminalSessionId, TerminalError> {
        let id = id.into();
        if self.sessions.contains_key(&id) {
            return Err(TerminalError::AlreadyOpen(id));
        }
        validate_terminal_dimensions(spec.rows, spec.cols)?;

        let size = PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = allocate_pty(size)?;

        let mut command = CommandBuilder::new(spec.resolved_program());
        command.args(&spec.args);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("CLICOLOR", "1");
        command.env_remove("NO_COLOR");
        // The API credential does not go into a terminal the API opened.
        //
        // `CommandBuilder` inherits this process's environment, and a bearer
        // that can open a session can name any `program` — so a shell started
        // through the API could read the token out of its own environment and
        // then drive every session, forge an actor, or start more programs. The
        // credential controls the terminal; it must not be inside one the
        // credential itself asked for. A session started locally still inherits
        // the operator's environment: that is theirs, and nested `afterminal`
        // needs it.
        if spec.api_requested {
            for name in API_CREDENTIAL_ENV {
                command.env_remove(name);
            }
        }
        // Before `spec.env`: a caller that explicitly names `AFUI_DELIVERY`
        // (or anything else) always wins over this manager-wide default.
        if let Some(delivery) = &self.afui_delivery {
            command.env(AFUI_DELIVERY_ENV, delivery);
        }
        for (name, value) in &spec.env {
            command.env(name, value);
        }
        if let Some(cwd) = &spec.cwd {
            command.cwd(cwd);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| TerminalError::Backend(error.to_string()))?;
        // The slave handle is no longer needed once the child holds it.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|error| TerminalError::Backend(error.to_string()))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Backend(error.to_string()))?;

        // Phase 2: Initialize the VT screen model with the specified dimensions.
        let parser =
            vt100::Parser::new_with_callbacks(spec.rows, spec.cols, 0, TerminalQueries::default());

        let shared = Arc::new(Mutex::new(Shared {
            ring: Vec::new(),
            ring_capacity: DEFAULT_RING_CAPACITY,
            subscribers: Vec::new(),
            status: TerminalSessionStatus::Running,
            parser,
            screen_seq: 0,
            last_output: Instant::now(),
            spec_title: spec.title.clone(),
            rows: spec.rows,
            cols: spec.cols,
            id: id.clone(),
            events: Arc::clone(&self.events),
            secret: None,
        }));

        let reader_shared = Arc::clone(&shared);
        let writer = Arc::new(Mutex::new(writer));
        let reader_writer = Arc::clone(&writer);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        let replies = match reader_shared.lock() {
                            Ok(mut state) => state.push_chunk(&buffer[..read]),
                            Err(_) => Vec::new(),
                        };
                        // Sent with no `Shared` lock held, so answering a query
                        // can never wait on a writer the manager is using.
                        if !replies.is_empty()
                            && let Ok(mut writer) = reader_writer.lock()
                        {
                            for reply in replies {
                                if writer.write_all(&reply).is_err() {
                                    break;
                                }
                            }
                            let _ = writer.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
            if let Ok(mut state) = reader_shared.lock()
                && matches!(state.status, TerminalSessionStatus::Running)
            {
                state.status = TerminalSessionStatus::Exited(None);
                // Phase 3: emit while still holding `Shared`, per the
                // `Shared` -> `EventBus` lock order.
                let session_id = state.id.clone();
                if let Ok(mut bus) = state.events.lock() {
                    bus.emit(session_id, TerminalEvent::ProcessExited { code: None });
                }
            }
        });

        self.sessions.insert(
            id.clone(),
            TerminalSession {
                master: pair.master,
                writer,
                child,
                shared,
                rows: spec.rows,
                cols: spec.cols,
                title: spec.title,
                input_leases: BTreeMap::new(),
                _reader: reader,
            },
        );

        // Phase 3: announce the new session once its state is ready. This is
        // a manager-only emission — no `Shared` lock is held here, so
        // `EventBus` is locked alone.
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(id.clone(), TerminalEvent::SessionOpened);
        }

        Ok(id)
    }

    /// Forward raw input bytes to a terminal session (keystrokes / agent writes).
    pub fn write(&mut self, id: &str, bytes: &[u8]) -> Result<(), TerminalError> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let mut writer = session
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Write one complete input chunk on behalf of an identified actor.
    ///
    /// Non-human actors must hold a shared or exclusive lease. Multiple shared
    /// holders can write, but each call remains atomic under the manager lock.
    /// Human input needs no lease and revokes a non-human exclusive lease
    /// before writing.
    pub fn write_as(
        &mut self,
        id: &str,
        actor: InputActor,
        lease_id: Option<&str>,
        bytes: &[u8],
    ) -> Result<(), TerminalError> {
        let authorization = match self.authorize_actor(id, &actor, lease_id) {
            Ok(authorization) => authorization,
            Err(error) => {
                self.emit_input_rejected(id, actor, &error);
                return Err(error);
            }
        };
        self.emit_preemptions(id, &actor, &authorization.preempted);

        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let mut writer = session
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        writer.write_all(bytes)?;
        writer.flush()?;

        // A secret is typed by a person, so this write is accepted — but
        // announcing it would publish how many bytes the secret is.
        if authorization.secret_active {
            return Ok(());
        }
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(
                id.to_string(),
                TerminalEvent::InputAccepted {
                    actor,
                    input_bytes: bytes.len(),
                    lease_id: authorization.lease_id,
                },
            );
        }
        Ok(())
    }

    /// Put a session into secret input mode.
    ///
    /// While it is on, the session stops publishing: no raw bytes to
    /// subscribers, nothing retained for replay, no screen content, and no
    /// event describing output or input volume. Non-human actors are refused
    /// input, signals, and leases. Only [`TerminalEvent::SecretInputStarted`]
    /// and [`TerminalEvent::SecretInputEnded`] cross the bus, so the gap is
    /// announced rather than silent.
    ///
    /// Any actor may enter — raising this shield is always the safe direction,
    /// and a prompt detector that spots `Password:` should be able to. Entering
    /// a window that is already open keeps the original owner and reason.
    pub fn enter_secret(
        &mut self,
        id: &str,
        actor: InputActor,
        reason: &str,
    ) -> Result<SecretInputStatus, TerminalError> {
        validate_input_actor(&actor)?;
        let reason = validate_secret_input_reason(reason)?;
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let started = {
            let mut state = session.shared.lock().map_err(|_| TerminalError::Poisoned)?;
            if state.secret.is_some() {
                None
            } else {
                let screen = state.parser.screen();
                let alt_screen = screen.alternate_screen();
                let screen_seq = state.screen_seq;
                state.secret = Some(SecretWindow {
                    actor: actor.clone(),
                    reason: reason.clone(),
                    since: Instant::now(),
                    screen_seq,
                    alt_screen,
                });
                Some(state.secret_status())
            }
        };
        let Some(status) = started else {
            return self.secret_input(id);
        };
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(
                id.to_string(),
                TerminalEvent::SecretInputStarted { actor, reason },
            );
        }
        Ok(status)
    }

    /// End secret input mode and resume publishing.
    ///
    /// Only a `Human` actor may do this. An agent can raise the shield but
    /// never lower it: if lowering it were automatable, an agent that wanted
    /// the person's secret would simply lower it and read the screen.
    /// Ending a window that is not open is a no-op.
    ///
    /// Ending is refused with [`TerminalError::SecretInputSettling`] while the
    /// session is still producing output — see [`SECRET_INPUT_SETTLE_MS`]. The
    /// caller retries; the failure direction is "stays private".
    pub fn exit_secret(
        &mut self,
        id: &str,
        actor: InputActor,
    ) -> Result<SecretInputStatus, TerminalError> {
        validate_input_actor(&actor)?;
        if !actor.kind.is_human() {
            return Err(TerminalError::SecretInputExitDenied {
                session_id: id.to_string(),
                actor,
            });
        }
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let ended = {
            let mut state = session.shared.lock().map_err(|_| TerminalError::Poisoned)?;
            if state.secret.is_some() {
                let quiet_for_ms = state.last_output.elapsed().as_millis() as u64;
                if quiet_for_ms < SECRET_INPUT_SETTLE_MS {
                    return Err(TerminalError::SecretInputSettling {
                        session_id: id.to_string(),
                        quiet_for_ms,
                    });
                }
            }
            let ended = state.secret.take().is_some();
            if ended {
                // The live parser necessarily saw echoed secret bytes so it
                // could keep terminal modes current. It cannot publish that
                // grid after the shield drops. Start from a clean authoritative
                // screen; the next program output repopulates it without ever
                // exposing the protected interval.
                state.parser = vt100::Parser::new_with_callbacks(
                    state.rows,
                    state.cols,
                    0,
                    TerminalQueries::default(),
                );
                state.screen_seq += 1;
            }
            ended
        };
        if ended && let Ok(mut bus) = self.events.lock() {
            bus.emit(id.to_string(), TerminalEvent::SecretInputEnded { actor });
        }
        Ok(SecretInputStatus::inactive())
    }

    /// Current secret input state of a session.
    pub fn secret_input(&self, id: &str) -> Result<SecretInputStatus, TerminalError> {
        let session = self
            .sessions
            .get(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let state = session.shared.lock().map_err(|_| TerminalError::Poisoned)?;
        Ok(state.secret_status())
    }

    /// Create or renew one shared or exclusive input lease.
    ///
    /// Omitting `lease_id` renews the actor's existing lease when present,
    /// otherwise it creates a new manager-generated id. A human lease request
    /// may preempt incompatible non-human leases; non-human actors never
    /// preempt one another.
    pub fn acquire_lease(
        &mut self,
        id: &str,
        actor: InputActor,
        mode: InputLeaseMode,
        ttl_ms: u64,
        lease_id: Option<&str>,
    ) -> Result<InputLease, TerminalError> {
        validate_input_actor(&actor)?;
        validate_input_lease_ttl(ttl_ms)?;
        // Phase 6: a lease is permission to act, so it is refused for the same
        // reason input is while a person is entering a secret here.
        if !actor.kind.is_human() && self.secret_input(id)?.active {
            return Err(TerminalError::SecretInputActive {
                session_id: id.to_string(),
                actor,
            });
        }
        self.purge_expired_leases(id)?;

        let resolved_lease_id = {
            let session = self
                .sessions
                .get(id)
                .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
            if let Some(requested_lease_id) = lease_id {
                let lease = session
                    .input_leases
                    .get(requested_lease_id)
                    .ok_or_else(|| TerminalError::InputLeaseNotFound {
                        session_id: id.to_string(),
                        lease_id: requested_lease_id.to_string(),
                    })?;
                if lease.actor != actor {
                    return Err(TerminalError::InputLeaseConflict {
                        session_id: id.to_string(),
                        actor,
                        held_by: Some(lease.actor.clone()),
                    });
                }
                Some(requested_lease_id.to_string())
            } else {
                session
                    .input_leases
                    .values()
                    .find(|lease| lease.actor == actor)
                    .map(|lease| lease.lease_id.clone())
            }
        };
        let resolved_lease_id = match resolved_lease_id {
            Some(lease_id) => lease_id,
            None => self.next_input_lease_id()?,
        };

        let now = Instant::now();
        let (lease, preempted) = {
            let session = self
                .sessions
                .get_mut(id)
                .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
            let preemptible_ids = session
                .input_leases
                .values()
                .filter(|lease| {
                    lease.lease_id != resolved_lease_id
                        && actor.kind.is_human()
                        && !lease.actor.kind.is_human()
                        && (mode == InputLeaseMode::Exclusive
                            || lease.mode == InputLeaseMode::Exclusive)
                })
                .map(|lease| lease.lease_id.clone())
                .collect::<Vec<_>>();

            let conflicting = session.input_leases.values().find(|lease| {
                lease.lease_id != resolved_lease_id
                    && !preemptible_ids.contains(&lease.lease_id)
                    && (mode == InputLeaseMode::Exclusive
                        || lease.mode == InputLeaseMode::Exclusive)
            });
            if let Some(conflicting) = conflicting {
                return Err(TerminalError::InputLeaseConflict {
                    session_id: id.to_string(),
                    actor,
                    held_by: Some(conflicting.actor.clone()),
                });
            }

            let preempted = preemptible_ids
                .into_iter()
                .filter_map(|lease_id| session.input_leases.remove(&lease_id))
                .collect::<Vec<_>>();
            let active = ActiveInputLease {
                lease_id: resolved_lease_id.clone(),
                actor: actor.clone(),
                mode,
                ttl_ms,
                expires_at: now + Duration::from_millis(ttl_ms),
            };
            let lease = active.snapshot(now);
            session.input_leases.insert(resolved_lease_id, active);
            (lease, preempted)
        };

        self.emit_preemptions(id, &actor, &preempted);
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(
                id.to_string(),
                TerminalEvent::InputLeaseAcquired {
                    lease: lease.clone(),
                },
            );
        }
        Ok(lease)
    }

    /// Return every active input lease in deterministic lease-id order.
    pub fn leases(&mut self, id: &str) -> Result<Vec<InputLease>, TerminalError> {
        self.purge_expired_leases(id)?;
        let session = self
            .sessions
            .get(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let now = Instant::now();
        Ok(session
            .input_leases
            .values()
            .map(|lease| lease.snapshot(now))
            .collect())
    }

    /// Release an input lease explicitly.
    pub fn release_lease(&mut self, id: &str, lease_id: &str) -> Result<(), TerminalError> {
        self.purge_expired_leases(id)?;
        let lease = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?
            .input_leases
            .remove(lease_id)
            .ok_or_else(|| TerminalError::InputLeaseNotFound {
                session_id: id.to_string(),
                lease_id: lease_id.to_string(),
            })?;
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(
                id.to_string(),
                TerminalEvent::InputLeaseReleased {
                    lease_id: lease.lease_id,
                    actor: lease.actor,
                    reason: InputLeaseReleaseReason::Released,
                },
            );
        }
        Ok(())
    }

    /// Resize a terminal session window.
    pub fn resize(&mut self, id: &str, rows: u16, cols: u16) -> Result<(), TerminalError> {
        validate_terminal_dimensions(rows, cols)?;
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        session
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| TerminalError::Backend(error.to_string()))?;
        session.rows = rows;
        session.cols = cols;

        // Phase 2: Resize the VT screen in place, preserving current content
        // (vt100 reflows both the normal and alternate grids).
        if let Ok(mut state) = session.shared.lock() {
            state.rows = rows;
            state.cols = cols;
            state.parser.screen_mut().set_size(rows, cols);
        }

        // Phase 3: emit after the `Shared` lock above is released, so
        // `EventBus` is locked alone here.
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(id.to_string(), TerminalEvent::Resized { rows, cols });
        }
        Ok(())
    }

    /// Deliver a real process signal to the terminal's current foreground job.
    ///
    /// Unix PTYs target the foreground process group reported by the kernel,
    /// so an active child command is interrupted instead of merely writing a
    /// control character to the PTY. Windows currently supports only
    /// [`TerminalSignal::Kill`] through the backend child-process handle.
    pub fn signal(&mut self, id: &str, signal: TerminalSignal) -> Result<(), TerminalError> {
        self.signal_inner(id, signal, None, None)
    }

    /// Deliver a process signal on behalf of an actor under the same lease
    /// rules as actor input.
    pub fn signal_as(
        &mut self,
        id: &str,
        actor: InputActor,
        lease_id: Option<&str>,
        signal: TerminalSignal,
    ) -> Result<(), TerminalError> {
        let authorization = self.authorize_actor(id, &actor, lease_id)?;
        self.emit_preemptions(id, &actor, &authorization.preempted);
        self.signal_inner(id, signal, Some(actor), authorization.lease_id)
    }

    fn signal_inner(
        &mut self,
        id: &str,
        signal: TerminalSignal,
        actor: Option<InputActor>,
        lease_id: Option<String>,
    ) -> Result<(), TerminalError> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        if let Some(exit) = session.child.try_wait()? {
            let code = i32::try_from(exit.exit_code()).ok();
            if let Ok(mut state) = session.shared.lock() {
                state.status = TerminalSessionStatus::Exited(code);
            }
            return Err(TerminalError::NotRunning(id.to_string()));
        }

        deliver_signal(session, id, signal)?;

        if let Ok(mut bus) = self.events.lock() {
            bus.emit(
                id.to_string(),
                TerminalEvent::SignalSent {
                    signal,
                    actor,
                    lease_id,
                },
            );
        }
        Ok(())
    }

    /// Kill a session's shell process and mark it exited.
    #[allow(dead_code)]
    pub fn kill(&mut self, id: &str) -> Result<(), TerminalError> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        session.child.kill()?;
        if let Ok(mut state) = session.shared.lock() {
            state.status = TerminalSessionStatus::Exited(None);
        }

        // Phase 3: emit after the `Shared` lock above is released, so
        // `EventBus` is locked alone here.
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(id.to_string(), TerminalEvent::ProcessExited { code: None });
        }
        Ok(())
    }

    /// Kill and remove a session from the manager.
    pub fn close(&mut self, id: &str) -> Result<(), TerminalError> {
        let process_exited = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?
            .child
            .try_wait()?
            .is_some();
        if !process_exited {
            self.kill(id)?;
        }
        self.sessions.remove(id);
        Ok(())
    }

    /// Current status of a session, reconciling any pending child exit.
    #[allow(dead_code)]
    pub fn status(&mut self, id: &str) -> Option<TerminalSessionStatus> {
        let session = self.sessions.get_mut(id)?;
        if let Ok(Some(exit)) = session.child.try_wait() {
            let code = i32::try_from(exit.exit_code()).ok();
            if let Ok(mut state) = session.shared.lock() {
                state.status = TerminalSessionStatus::Exited(code);
            }
        }
        session.shared.lock().ok().map(|state| state.status.clone())
    }

    /// Host-local metadata for the space snapshot overlay (never the scrollback).
    pub fn metadata(&self, id: &str) -> Option<TerminalSessionMeta> {
        let session = self.sessions.get(id)?;
        let (status, secret_input) = session
            .shared
            .lock()
            .ok()
            .map(|state| (state.status.clone(), state.secret.is_some()))
            .unwrap_or((TerminalSessionStatus::Running, false));
        Some(TerminalSessionMeta {
            status,
            rows: session.rows,
            cols: session.cols,
            title: session.title.clone(),
            secret_input,
        })
    }

    /// Subscribe to a session's byte stream: current scrollback + future chunks.
    pub fn subscribe(&self, id: &str) -> Result<TerminalSubscription, TerminalError> {
        let session = self
            .sessions
            .get(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let mut state = session.shared.lock().map_err(|_| TerminalError::Poisoned)?;
        let (tx, rx) = channel();
        state.subscribers.push(tx);
        Ok(TerminalSubscription {
            backlog: state.ring.clone(),
            receiver: rx,
        })
    }

    /// Subscribe to the global (multiplexed) event stream: recent events
    /// across every session, plus a receiver of subsequent envelopes tagged
    /// by `session_id`. There is no per-session subscribe in this phase —
    /// callers that want one session's events filter the stream themselves.
    pub fn subscribe_events(&self) -> EventSubscription {
        let (tx, rx) = channel();
        let backlog = match self.events.lock() {
            Ok(mut bus) => {
                bus.subscribers.push(tx);
                bus.backlog.clone()
            }
            Err(_) => Vec::new(),
        };
        EventSubscription {
            backlog,
            receiver: rx,
        }
    }

    /// True if a session with this id is open.
    #[allow(dead_code)]
    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    /// Ids of all open sessions.
    pub fn ids(&self) -> Vec<TerminalSessionId> {
        let mut ids = self.sessions.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        ids
    }

    /// Get a snapshot of the current VT screen state. Returns None if the session
    /// does not exist or if the shared lock is poisoned.
    pub fn screen(&self, id: &str) -> Option<ScreenSnapshot> {
        let session = self.sessions.get(id)?;
        let state = session.shared.lock().ok()?;
        Some(state.snapshot())
    }

    fn authorize_actor(
        &mut self,
        id: &str,
        actor: &InputActor,
        lease_id: Option<&str>,
    ) -> Result<ActorAuthorization, TerminalError> {
        validate_input_actor(actor)?;
        self.purge_expired_leases(id)?;
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
        let secret_active = session
            .shared
            .lock()
            .map(|state| state.secret.is_some())
            .unwrap_or(false);
        // Phase 6: a person is entering a secret here. Every non-human actor
        // is suspended for the duration — it may not type into this session,
        // and (see `Shared::snapshot`) it cannot read it either.
        if secret_active && !actor.kind.is_human() {
            return Err(TerminalError::SecretInputActive {
                session_id: id.to_string(),
                actor: actor.clone(),
            });
        }

        if actor.kind.is_human() {
            let accepted_lease_id = lease_id.and_then(|lease_id| {
                session
                    .input_leases
                    .get(lease_id)
                    .filter(|lease| lease.actor == *actor)
                    .map(|lease| lease.lease_id.clone())
            });
            let preempted_ids = session
                .input_leases
                .values()
                .filter(|lease| {
                    !lease.actor.kind.is_human() && lease.mode == InputLeaseMode::Exclusive
                })
                .map(|lease| lease.lease_id.clone())
                .collect::<Vec<_>>();
            let preempted = preempted_ids
                .into_iter()
                .filter_map(|lease_id| session.input_leases.remove(&lease_id))
                .collect();
            return Ok(ActorAuthorization {
                lease_id: accepted_lease_id,
                preempted,
                secret_active,
            });
        }

        let lease_id = lease_id.ok_or_else(|| TerminalError::InputLeaseRequired {
            session_id: id.to_string(),
            actor: actor.clone(),
        })?;
        let lease = session.input_leases.get(lease_id).ok_or_else(|| {
            TerminalError::InputLeaseNotFound {
                session_id: id.to_string(),
                lease_id: lease_id.to_string(),
            }
        })?;
        if lease.actor != *actor {
            return Err(TerminalError::InputLeaseConflict {
                session_id: id.to_string(),
                actor: actor.clone(),
                held_by: Some(lease.actor.clone()),
            });
        }
        Ok(ActorAuthorization {
            lease_id: Some(lease.lease_id.clone()),
            preempted: Vec::new(),
            secret_active,
        })
    }

    fn purge_expired_leases(&mut self, id: &str) -> Result<(), TerminalError> {
        let expired = {
            let session = self
                .sessions
                .get_mut(id)
                .ok_or_else(|| TerminalError::NotFound(id.to_string()))?;
            let now = Instant::now();
            let expired_ids = session
                .input_leases
                .values()
                .filter(|lease| lease.expires_at <= now)
                .map(|lease| lease.lease_id.clone())
                .collect::<Vec<_>>();
            expired_ids
                .into_iter()
                .filter_map(|lease_id| session.input_leases.remove(&lease_id))
                .collect::<Vec<_>>()
        };
        if let Ok(mut bus) = self.events.lock() {
            for lease in expired {
                bus.emit(
                    id.to_string(),
                    TerminalEvent::InputLeaseReleased {
                        lease_id: lease.lease_id,
                        actor: lease.actor,
                        reason: InputLeaseReleaseReason::Expired,
                    },
                );
            }
        }
        Ok(())
    }

    fn emit_preemptions(&self, id: &str, by_actor: &InputActor, preempted: &[ActiveInputLease]) {
        if let Ok(mut bus) = self.events.lock() {
            for lease in preempted {
                bus.emit(
                    id.to_string(),
                    TerminalEvent::InputPreempted {
                        previous_actor: lease.actor.clone(),
                        by_actor: by_actor.clone(),
                        lease_id: lease.lease_id.clone(),
                    },
                );
                bus.emit(
                    id.to_string(),
                    TerminalEvent::InputLeaseReleased {
                        lease_id: lease.lease_id.clone(),
                        actor: lease.actor.clone(),
                        reason: InputLeaseReleaseReason::HumanPreempted,
                    },
                );
            }
        }
    }

    fn emit_input_rejected(&self, id: &str, actor: InputActor, error: &TerminalError) {
        let reason = match error {
            TerminalError::InputLeaseRequired { .. } => InputRejectionReason::LeaseRequired,
            TerminalError::InputLeaseNotFound { .. } => InputRejectionReason::LeaseNotFound,
            TerminalError::InputLeaseConflict { .. } => InputRejectionReason::LeaseConflict,
            TerminalError::SecretInputActive { .. } => InputRejectionReason::SecretInputActive,
            _ => return,
        };
        if let Ok(mut bus) = self.events.lock() {
            bus.emit(
                id.to_string(),
                TerminalEvent::InputRejected { actor, reason },
            );
        }
    }

    fn next_input_lease_id(&mut self) -> Result<String, TerminalError> {
        self.next_lease_seq = self
            .next_lease_seq
            .checked_add(1)
            .ok_or_else(|| TerminalError::Backend("input lease sequence exhausted".to_string()))?;
        Ok(format!("lease_{}", self.next_lease_seq))
    }
}

fn validate_input_actor(actor: &InputActor) -> Result<(), TerminalError> {
    if actor.id.is_empty() || actor.id.len() > 128 {
        return Err(TerminalError::InvalidInputActor(
            "id must contain 1-128 ASCII characters".to_string(),
        ));
    }
    let mut bytes = actor.id.bytes();
    let Some(first) = bytes.next() else {
        return Err(TerminalError::InvalidInputActor(
            "id must not be empty".to_string(),
        ));
    };
    if !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(TerminalError::InvalidInputActor(
            "id must start with an ASCII letter or digit and contain only letters, digits, dot, underscore, or hyphen"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_terminal_dimensions(rows: u16, cols: u16) -> Result<(), TerminalError> {
    if rows < MIN_TERMINAL_DIMENSION
        || cols < MIN_TERMINAL_DIMENSION
        || rows > MAX_TERMINAL_DIMENSION
        || cols > MAX_TERMINAL_DIMENSION
    {
        return Err(TerminalError::InvalidDimensions { rows, cols });
    }
    Ok(())
}

/// The reason republished on the event stream, so it is bounded and single
/// line: it is operator context, never a place to park terminal content.
fn validate_secret_input_reason(reason: &str) -> Result<String, TerminalError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_SECRET_INPUT_REASON_LEN {
        return Err(TerminalError::InvalidSecretInputReason(format!(
            "reason must contain 1-{MAX_SECRET_INPUT_REASON_LEN} characters"
        )));
    }
    if reason.chars().any(char::is_control) {
        return Err(TerminalError::InvalidSecretInputReason(
            "reason must not contain control characters".to_string(),
        ));
    }
    Ok(reason.to_string())
}

fn validate_input_lease_ttl(ttl_ms: u64) -> Result<(), TerminalError> {
    if ttl_ms == 0 || ttl_ms > MAX_INPUT_LEASE_TTL_MS {
        return Err(TerminalError::InvalidInputLeaseTtl { ttl_ms });
    }
    Ok(())
}

#[cfg(unix)]
fn deliver_signal(
    session: &mut TerminalSession,
    id: &str,
    signal: TerminalSignal,
) -> Result<(), TerminalError> {
    let process_group_id = session
        .master
        .process_group_leader()
        .or_else(|| {
            session
                .child
                .process_id()
                .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        })
        .ok_or_else(|| {
            TerminalError::Backend(format!(
                "terminal session `{id}` has no process group identifier"
            ))
        })?;

    // SAFETY: `process_group_id` is a positive id obtained from the PTY kernel
    // state (or the portable-pty child, which creates a new Unix session), and
    // `unix_number` returns one of three fixed, valid signal constants.
    let result = unsafe { libc::killpg(process_group_id, signal.unix_number()) };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Err(TerminalError::NotRunning(id.to_string()));
    }
    Err(TerminalError::Io(error))
}

#[cfg(not(unix))]
fn deliver_signal(
    session: &mut TerminalSession,
    _id: &str,
    signal: TerminalSignal,
) -> Result<(), TerminalError> {
    match signal {
        TerminalSignal::Kill => session.child.kill().map_err(TerminalError::Io),
        TerminalSignal::Interrupt | TerminalSignal::Terminate => {
            Err(TerminalError::UnsupportedSignal(signal))
        }
    }
}

/// Shell scaffolding shared by the tests in this crate.
///
/// Almost every test here has the runtime as its subject — leases, events,
/// secret mode, the screen model — and wants a shell only as something on the
/// far end that prints when written to. Naming one per platform is what keeps
/// those tests running on both, so the Windows binary this crate ships is
/// covered by the same suite instead of by none of it.
///
/// Windows runs `cmd.exe` with delayed expansion on, because that is the only
/// mode where an *unset* variable expands to nothing. Plain `%VAR%` is left as
/// literal text, so a test asserting a scrubbed variable would read back
/// `TOKEN=[%VAR%]` and could not tell "expanded to empty" from "never expanded
/// at all" — it would fail while the scrubbing it checks works fine. With
/// `!VAR!` both platforms produce the same `TOKEN=[]`, so the assertions stay
/// identical rather than forking per platform.
///
/// Note that the shell echoes what is typed into it, so a marker a test waits
/// for must not appear in the command that produces it. That is why the
/// variable reporters print `LABEL=[...]` from a placeholder rather than
/// spelling the expected output in the command line.
#[cfg(test)]
pub(crate) mod test_shell {
    use super::TerminalOpenSpec;

    /// The shell binary, chosen for deterministic echo rather than taken from
    /// the environment the test happens to run in.
    pub(crate) fn program() -> String {
        if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
        } else {
            "/bin/sh".to_string()
        }
    }

    /// Arguments that start it interactively.
    pub(crate) fn args() -> Vec<String> {
        if cfg!(windows) {
            vec!["/V:ON".to_string()]
        } else {
            Vec::new()
        }
    }

    /// An open spec for an interactive shell, for tests that then write to it.
    pub(crate) fn spec() -> TerminalOpenSpec {
        TerminalOpenSpec {
            program: Some(program()),
            args: args(),
            ..TerminalOpenSpec::default()
        }
    }

    /// Input that makes the shell print `marker` on a line of its own.
    pub(crate) fn echo(marker: &str) -> Vec<u8> {
        if cfg!(windows) {
            format!("echo {marker}\r\n").into_bytes()
        } else {
            format!("printf '{marker}\\n'\n").into_bytes()
        }
    }

    /// Input that makes the shell print `label=[value]` for an environment
    /// variable, printing `label=[]` when it is unset — on both platforms.
    pub(crate) fn echo_env(label: &str, var: &str) -> Vec<u8> {
        if cfg!(windows) {
            format!("echo {label}=[!{var}!]\r\n").into_bytes()
        } else {
            format!("printf '{label}=[%s]\\n' \"${var}\"\n").into_bytes()
        }
    }

    /// Arguments that print `marker` once and exit, without an interactive
    /// session in between.
    pub(crate) fn echo_once_args(marker: &str) -> Vec<String> {
        if cfg!(windows) {
            vec!["/C".to_string(), format!("echo {marker}")]
        } else {
            vec!["-c".to_string(), format!("printf '{marker}\\n'")]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // Reads from a subscription until `marker` appears or `timeout` elapses.
    fn wait_for(subscription: &TerminalSubscription, marker: &str, timeout: Duration) -> bool {
        let mut accumulated = subscription.backlog.clone();
        if String::from_utf8_lossy(&accumulated).contains(marker) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match subscription
                .receiver
                .recv_timeout(Duration::from_millis(200))
            {
                Ok(chunk) => {
                    accumulated.extend_from_slice(&chunk);
                    if String::from_utf8_lossy(&accumulated).contains(marker) {
                        return true;
                    }
                }
                Err(_) => continue,
            }
        }
        false
    }

    // Reads from a subscription until `marker` appears, and returns everything
    // seen up to that point. The marker is a barrier: because one reader thread
    // feeds this stream in order, anything the PTY produced before the marker
    // has already been offered by the time the marker arrives, so text missing
    // from the return value was withheld rather than merely late.
    fn drain_until(
        subscription: &TerminalSubscription,
        marker: &str,
        timeout: Duration,
    ) -> Option<String> {
        let mut accumulated = subscription.backlog.clone();
        let deadline = Instant::now() + timeout;
        loop {
            let text = String::from_utf8_lossy(&accumulated).into_owned();
            if text.contains(marker) {
                return Some(text);
            }
            if Instant::now() >= deadline {
                return None;
            }
            if let Ok(chunk) = subscription
                .receiver
                .recv_timeout(Duration::from_millis(200))
            {
                accumulated.extend_from_slice(&chunk);
            }
        }
    }

    // Collects envelopes until `predicate` matches, and returns everything
    // collected including the matching envelope. The same barrier argument as
    // `drain_until` applies: the bus is a single ordered stream.
    fn drain_events_until(
        subscription: &EventSubscription,
        timeout: Duration,
        mut predicate: impl FnMut(&EventEnvelope) -> bool,
    ) -> Option<Vec<EventEnvelope>> {
        let mut collected = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(envelope) = subscription
                .receiver
                .recv_timeout(Duration::from_millis(200))
            {
                let matched = predicate(&envelope);
                collected.push(envelope);
                if matched {
                    return Some(collected);
                }
            }
        }
        None
    }

    // Polls an event subscription's receiver until `predicate` matches an
    // envelope or `timeout` elapses. Mirrors `wait_for`'s bounded-poll style,
    // but for the (multiplexed) event stream instead of the raw byte stream.
    fn wait_for_event(
        subscription: &EventSubscription,
        timeout: Duration,
        mut predicate: impl FnMut(&EventEnvelope) -> bool,
    ) -> Option<EventEnvelope> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(envelope) = subscription
                .receiver
                .recv_timeout(Duration::from_millis(200))
                && predicate(&envelope)
            {
                return Some(envelope);
            }
        }
        None
    }

    #[test]
    fn open_streams_host_written_marker() -> Result<(), TerminalError> {
        // The shell and the command it is given are trusted host-side test
        // setup; see `test_shell` for why they are chosen per platform.
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t1", test_shell::spec())?;
        let subscription = manager.subscribe(&id)?;
        manager.write(&id, &test_shell::echo("TERMINAL_READY"))?;
        assert!(
            wait_for(&subscription, "TERMINAL_READY", Duration::from_secs(5)),
            "expected TERMINAL_READY in the session byte stream"
        );
        Ok(())
    }

    #[test]
    fn resize_updates_metadata() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t2", test_shell::spec())?;
        manager.resize(&id, 50, 160)?;
        let meta = manager
            .metadata(&id)
            .ok_or(TerminalError::NotFound(id.clone()))?;
        assert_eq!(meta.rows, 50);
        assert_eq!(meta.cols, 160);
        Ok(())
    }

    #[test]
    fn one_cell_geometry_is_rejected_before_it_can_poison_the_reader() -> Result<(), TerminalError>
    {
        let mut manager = TerminalSessionManager::new();
        assert!(matches!(
            manager.open(
                "one_row",
                TerminalOpenSpec {
                    rows: 1,
                    cols: 80,
                    ..TerminalOpenSpec::default()
                }
            ),
            Err(TerminalError::InvalidDimensions { rows: 1, cols: 80 })
        ));

        let id = manager.open("safe_geometry", test_shell::spec())?;
        assert!(matches!(
            manager.resize(&id, 1, 35),
            Err(TerminalError::InvalidDimensions { rows: 1, cols: 35 })
        ));
        let meta = manager
            .metadata(&id)
            .ok_or(TerminalError::NotFound(id.clone()))?;
        assert_eq!((meta.rows, meta.cols), (24, 80));

        let subscription = manager.subscribe(&id)?;
        manager.write(&id, &test_shell::echo("STILL_STREAMING"))?;
        assert!(
            wait_for(&subscription, "STILL_STREAMING", Duration::from_secs(5)),
            "a refused resize must leave the reader thread alive"
        );
        Ok(())
    }

    #[test]
    fn kill_marks_exited() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t3", test_shell::spec())?;
        manager.kill(&id)?;
        assert!(matches!(
            manager.status(&id),
            Some(TerminalSessionStatus::Exited(_))
        ));
        Ok(())
    }

    #[test]
    fn duplicate_open_is_rejected() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        manager.open("dup", TerminalOpenSpec::default())?;
        assert!(matches!(
            manager.open("dup", TerminalOpenSpec::default()),
            Err(TerminalError::AlreadyOpen(_))
        ));
        Ok(())
    }

    // Serializes the two tests below that mutate the *process* environment
    // (`std::env::set_var`/`remove_var`), so they cannot race each other or
    // any other test that happens to read the same variable name.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(windows)]
    #[test]
    fn diagnose_credential_scrub() {
        const VAR: &str = "AFTERMINAL_API_ACCESS_TOKEN_SECRET";
        fn collect(sub: &TerminalSubscription, ms: u64) -> String {
            let mut acc = sub.backlog.clone();
            let deadline = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < deadline {
                if let Ok(chunk) = sub.receiver.recv_timeout(Duration::from_millis(100)) {
                    acc.extend_from_slice(&chunk);
                }
            }
            String::from_utf8_lossy(&acc).escape_debug().to_string()
        }
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe { std::env::set_var(VAR, "the-api-bearer") };
        let mut report = String::new();
        let mut manager = TerminalSessionManager::new();

        let scrubbed = manager
            .open(
                "diag_scrub",
                TerminalOpenSpec {
                    api_requested: true,
                    ..test_shell::spec()
                },
            )
            .expect("scrubbed session opens");
        let sub_a = manager.subscribe(&scrubbed).expect("subscribe a");
        manager
            .write(&scrubbed, &test_shell::echo_env("TOKEN", VAR))
            .expect("write a");
        report.push_str(&format!("WROTE={:?}\n", String::from_utf8_lossy(&test_shell::echo_env("TOKEN", VAR))));
        report.push_str(&format!("SCRUBBED_RAW=[{}]\n", collect(&sub_a, 4000)));
        report.push_str(&format!("SCRUBBED_SCREEN={:?}\n", manager.screen(&scrubbed).map(|s| s.lines)));

        let kept = manager
            .open("diag_keep", test_shell::spec())
            .expect("inheriting session opens");
        let sub_b = manager.subscribe(&kept).expect("subscribe b");
        manager
            .write(&kept, &test_shell::echo_env("TOKEN", VAR))
            .expect("write b");
        report.push_str(&format!("KEPT_RAW=[{}]\n", collect(&sub_b, 4000)));
        unsafe { std::env::remove_var(VAR) };
        panic!("CREDENTIAL SCRUB DIAGNOSTIC\n{report}");
    }

    fn query_replies(parser: &mut vt100::Parser<TerminalQueries>) -> Vec<String> {
        std::mem::take(&mut parser.callbacks_mut().replies)
            .into_iter()
            .map(|reply| String::from_utf8_lossy(&reply).escape_debug().to_string())
            .collect()
    }

    /// A program can ask the terminal where the cursor is and then wait for the
    /// answer. Windows makes that unavoidable: ConPTY opens by asking, and
    /// emits nothing at all — no banner, no prompt, no echo of what is typed —
    /// until it is answered. A terminal silent here is not a degraded terminal,
    /// it is a dead one, which is what the Windows binary was.
    #[test]
    fn the_terminal_answers_where_its_cursor_is() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, TerminalQueries::default());
        parser.process(b"\x1b[6n");
        assert_eq!(
            query_replies(&mut parser),
            vec!["\\u{1b}[1;1R".to_string()],
            "an untouched screen reports the cursor at the origin, 1-based"
        );

        // The answer describes the screen as it is now, not as it opened.
        parser.process(b"hello\r\nworld");
        parser.process(b"\x1b[6n");
        assert_eq!(
            query_replies(&mut parser),
            vec!["\\u{1b}[2;6R".to_string()],
            "the reported position must follow the cursor"
        );
    }

    /// The other half of the same conversation, and the limit of it: this
    /// terminal answers what it can and stays quiet about what it cannot, since
    /// a program that believes a wrong answer is worse off than one that gets
    /// none.
    #[test]
    fn the_terminal_reports_itself_healthy_and_declines_what_it_cannot_answer() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, TerminalQueries::default());
        parser.process(b"\x1b[5n");
        assert_eq!(
            query_replies(&mut parser),
            vec!["\\u{1b}[0n".to_string()],
            "a status query is answered with no malfunction"
        );

        // `ESC[?6n` asks about extensions this terminal does not implement.
        parser.process(b"\x1b[?6n");
        assert!(
            query_replies(&mut parser).is_empty(),
            "a private query must not be answered as if it were the standard one"
        );

        // And an unrelated sequence is not mistaken for a question.
        parser.process(b"\x1b[2J");
        assert!(query_replies(&mut parser).is_empty());
    }

    /// A caller that names no program must get a shell this platform can
    /// actually execute.
    ///
    /// The Windows binary this crate ships used to fall back to `/bin/sh` — a
    /// path that cannot exist there — so every session opened without an
    /// explicit program failed with "the system cannot find the path
    /// specified", which is to say the default way to use it did not work at
    /// all. `open` returning an error is the whole symptom, so this test needs
    /// no output to catch it.
    #[test]
    fn a_session_naming_no_program_opens_on_the_platform_shell() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t_default_shell", TerminalOpenSpec::default())?;
        assert!(
            matches!(
                manager.status(&id),
                Some(TerminalSessionStatus::Running | TerminalSessionStatus::Exited(_))
            ),
            "a session opened with no program named must be a started process"
        );
        manager.close(&id)?;
        Ok(())
    }

    /// `SHELL` is a POSIX convention, and on Windows the one common way it is
    /// set — a session started from Git Bash or another MSYS shell — sets it to
    /// a path in MSYS's filesystem namespace that `CreateProcessW` cannot
    /// resolve. Honouring it there would break the default for exactly the
    /// people who have it set, so Windows asks `COMSPEC` instead.
    #[test]
    fn the_default_shell_never_comes_from_an_msys_shell_variable() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let restore = std::env::var("SHELL").ok();
        // SAFETY: serialized by ENV_TEST_LOCK, and restored below.
        unsafe { std::env::set_var("SHELL", "/usr/bin/bash") };
        let resolved = default_shell();
        // SAFETY: same guard.
        unsafe {
            match &restore {
                Some(value) => std::env::set_var("SHELL", value),
                None => std::env::remove_var("SHELL"),
            }
        }
        if cfg!(windows) {
            assert_ne!(
                resolved, "/usr/bin/bash",
                "Windows must not take its default shell from SHELL"
            );
            assert!(
                !resolved.starts_with('/'),
                "a Windows default shell must not be a POSIX path, got {resolved}"
            );
        } else {
            assert_eq!(
                resolved, "/usr/bin/bash",
                "a POSIX platform honours the shell the person actually uses"
            );
        }
    }

    #[test]
    fn open_sets_afui_delivery_when_configured() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new().with_afui_delivery("session");
        let id = manager.open("afui_delivery_set", test_shell::spec())?;
        let subscription = manager.subscribe(&id)?;
        manager.write(&id, &test_shell::echo_env("AFUI_DELIVERY", "AFUI_DELIVERY"))?;
        assert!(
            wait_for(
                &subscription,
                "AFUI_DELIVERY=[session]",
                Duration::from_secs(5)
            ),
            "expected the PTY child to see AFUI_DELIVERY=session"
        );
        Ok(())
    }

    #[test]
    fn open_leaves_afui_delivery_unset_by_default_and_preserves_a_pre_existing_export()
    -> Result<(), TerminalError> {
        // `--mode window` must do nothing here, specifically so a value a
        // person already exported in their own shell survives untouched —
        // setting it to a fixed "window" would stomp that.
        const VAR: &str = "AFUI_DELIVERY";
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: serialized by ENV_TEST_LOCK; no other test in this crate
        // reads or writes this name.
        unsafe { std::env::set_var(VAR, "a-persons-own-export") };
        let mut manager = TerminalSessionManager::new(); // no with_afui_delivery
        let opened = manager.open("afui_delivery_unset", test_shell::spec());
        let result = opened.and_then(|id| {
            let subscription = manager.subscribe(&id)?;
            manager.write(&id, &test_shell::echo_env("AFUI_DELIVERY", "AFUI_DELIVERY"))?;
            Ok(wait_for(
                &subscription,
                "AFUI_DELIVERY=[a-persons-own-export]",
                Duration::from_secs(5),
            ))
        });
        // SAFETY: same guard.
        unsafe { std::env::remove_var(VAR) };
        assert!(
            result?,
            "expected a pre-existing AFUI_DELIVERY export to survive a manager with no delivery configured"
        );
        Ok(())
    }

    #[test]
    fn spec_env_overrides_manager_afui_delivery() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new().with_afui_delivery("session");
        let id = manager.open(
            "afui_delivery_override",
            TerminalOpenSpec {
                env: BTreeMap::from([("AFUI_DELIVERY".to_string(), "window".to_string())]),
                ..test_shell::spec()
            },
        )?;
        let subscription = manager.subscribe(&id)?;
        manager.write(&id, &test_shell::echo_env("AFUI_DELIVERY", "AFUI_DELIVERY"))?;
        assert!(
            wait_for(
                &subscription,
                "AFUI_DELIVERY=[window]",
                Duration::from_secs(5)
            ),
            "expected an explicit spec.env entry to win over the manager-wide default"
        );
        Ok(())
    }

    #[test]
    fn open_inherits_parent_process_env_vars() -> Result<(), TerminalError> {
        // This is not a test of any afterminal feature — it pins the
        // *assumption* `AFUI_DELIVERY` propagation (and `with_afui_delivery`
        // above) relies on: portable-pty's `CommandBuilder` starts from
        // `std::env::vars_os()` with no `env_clear`, so a PTY child inherits
        // the whole parent process environment unless told otherwise. That is
        // a third-party crate's default, not a contract this crate controls.
        // If it ever changes, the visible symptom is a command popping an
        // unattended window on someone's laptop while they are using it from
        // their phone — almost impossible to trace back here. This test is
        // the tripwire.
        const VAR: &str = "AFTERMINAL_TEST_PARENT_ENV_MARKER";
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: serialized by ENV_TEST_LOCK; no other test in this crate
        // reads or writes this name.
        unsafe { std::env::set_var(VAR, "seen-by-child") };
        let mut manager = TerminalSessionManager::new();
        let opened = manager.open("env_inherit", test_shell::spec());
        let result = opened.and_then(|id| {
            let subscription = manager.subscribe(&id)?;
            manager.write(&id, &test_shell::echo_env(VAR, VAR))?;
            Ok(wait_for(
                &subscription,
                &format!("{VAR}=[seen-by-child]"),
                Duration::from_secs(5),
            ))
        });
        // SAFETY: same guard.
        unsafe { std::env::remove_var(VAR) };
        assert!(
            result?,
            "expected the PTY child to inherit {VAR} from the parent process"
        );
        Ok(())
    }

    #[test]
    fn an_api_opened_session_does_not_inherit_the_api_credential() -> Result<(), TerminalError> {
        // The bearer that opens sessions can name any program, so a shell it
        // starts must not be able to read that bearer out of its own
        // environment and then drive every session on the host.
        const VAR: &str = "AFTERMINAL_API_ACCESS_TOKEN_SECRET";
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: serialized by ENV_TEST_LOCK.
        unsafe { std::env::set_var(VAR, "the-api-bearer") };

        let mut manager = TerminalSessionManager::new();
        let opened = manager.open(
            "api_env_scrub",
            TerminalOpenSpec {
                api_requested: true,
                ..test_shell::spec()
            },
        );
        let scrubbed = opened.and_then(|id| {
            let subscription = manager.subscribe(&id)?;
            manager.write(&id, &test_shell::echo_env("TOKEN", VAR))?;
            Ok(wait_for(&subscription, "TOKEN=[]", Duration::from_secs(5)))
        });

        // A session the operator starts still inherits their environment —
        // that is what nested `afterminal ui` runs on.
        let inherited = manager
            .open("local_env_keeps", test_shell::spec())
            .and_then(|id| {
                let subscription = manager.subscribe(&id)?;
                manager.write(&id, &test_shell::echo_env("TOKEN", VAR))?;
                Ok(wait_for(
                    &subscription,
                    "TOKEN=[the-api-bearer]",
                    Duration::from_secs(5),
                ))
            });

        // SAFETY: same guard.
        unsafe { std::env::remove_var(VAR) };
        assert!(
            scrubbed?,
            "a session opened over the API must not carry the API credential"
        );
        assert!(
            inherited?,
            "a locally opened session still inherits the operator's environment"
        );
        Ok(())
    }

    #[test]
    fn program_arguments_run_and_completed_session_can_close() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open(
            "program_args",
            TerminalOpenSpec {
                args: test_shell::echo_once_args("PROGRAM_ARGS"),
                ..test_shell::spec()
            },
        )?;
        let subscription = manager.subscribe(&id)?;
        assert!(
            wait_for(&subscription, "PROGRAM_ARGS", Duration::from_secs(5)),
            "expected direct program output in the session byte stream"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(manager.status(&id), Some(TerminalSessionStatus::Exited(_))) {
            assert!(
                Instant::now() < deadline,
                "direct program did not exit in time"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        manager.close(&id)?;
        assert!(!manager.contains(&id));
        Ok(())
    }

    #[test]
    fn screen_snapshot_contains_output() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t_screen", test_shell::spec())?;
        manager.write(&id, &test_shell::echo("PHASE2"))?;

        // Poll until the output appears in the screen snapshot.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(snapshot) = manager.screen(&id) {
                let combined = snapshot.lines.join("\n");
                if combined.contains("PHASE2") {
                    assert_eq!(snapshot.rows, 24);
                    assert_eq!(snapshot.cols, 80);
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                if let Some(snapshot) = manager.screen(&id) {
                    panic!("Expected PHASE2 in screen, got: {:?}", snapshot.lines);
                }
                panic!("Failed to get screen snapshot");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn screen_snapshot_reflects_resize() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open(
            "t_resize",
            TerminalOpenSpec {
                rows: 24,
                cols: 80,
                ..test_shell::spec()
            },
        )?;
        manager.write(&id, &test_shell::echo("test"))?;

        // Wait for initial output to stabilize.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(snapshot) = manager.screen(&id)
                && !snapshot.lines.is_empty()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Resize the terminal.
        manager.resize(&id, 50, 160)?;

        // Verify the snapshot reflects the new dimensions AND preserves content:
        // an in-place resize must not clear the screen. Both conditions are
        // retried across the same deadline — the reader thread may still be
        // catching up on the write above when dimensions first flip, so the
        // content check must not be a one-shot assert on the first iteration
        // where dimensions happen to match.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(snapshot) = manager.screen(&id)
                && snapshot.rows == 50
                && snapshot.cols == 160
                && snapshot.lines.join("\n").contains("test")
            {
                return Ok(());
            }
            if Instant::now() > deadline {
                if let Some(snapshot) = manager.screen(&id) {
                    panic!(
                        "Resize/content not reflected: got {}x{} lines={:?}, expected 50x160 containing \"test\"",
                        snapshot.rows, snapshot.cols, snapshot.lines
                    );
                }
                panic!("Failed to get screen snapshot after resize");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn screen_snapshot_quiescence() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t_quiescent", test_shell::spec())?;
        manager.write(&id, &test_shell::echo("test"))?;

        // Wait for output and verify activity is recent.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(snapshot) = manager.screen(&id) {
                let combined = snapshot.lines.join("\n");
                if combined.contains("test") {
                    assert!(
                        !snapshot.activity.quiescent,
                        "Should not be quiescent immediately after output"
                    );
                    break;
                }
            }
            if Instant::now() > deadline {
                panic!("Failed to find output in screen");
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Now wait for quiescence (500ms+ of inactivity).
        std::thread::sleep(Duration::from_millis(600));
        if let Some(snapshot) = manager.screen(&id) {
            assert!(
                snapshot.activity.quiescent,
                "Should be quiescent after 600ms of inactivity"
            );
        }
        Ok(())
    }

    #[test]
    fn events_stream_output_and_screen_changed() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t_events", test_shell::spec())?;
        // Subscribe after open: `subscribe_events` has no per-session backlog
        // guarantee for events emitted before subscribing, so this test opens
        // first, subscribes, then writes and asserts only on events produced
        // by that write.
        let subscription = manager.subscribe_events();
        manager.write(&id, &test_shell::echo("EVT"))?;

        let mut last_seq = 0u64;
        let mut seen_output_chunk = false;
        let mut seen_screen_changed = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !(seen_output_chunk && seen_screen_changed) {
            if let Ok(envelope) = subscription
                .receiver
                .recv_timeout(Duration::from_millis(200))
            {
                assert!(
                    envelope.seq > last_seq,
                    "seq must strictly increase across the global stream"
                );
                last_seq = envelope.seq;
                assert_eq!(envelope.session_id, id);
                match envelope.event {
                    TerminalEvent::OutputChunk { .. } => seen_output_chunk = true,
                    TerminalEvent::ScreenChanged { .. } => seen_screen_changed = true,
                    _ => {}
                }
            }
        }
        assert!(seen_output_chunk, "expected an OutputChunk event");
        assert!(seen_screen_changed, "expected a ScreenChanged event");
        Ok(())
    }

    #[test]
    fn resize_emits_resized_event() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t_resize_event", test_shell::spec())?;
        let subscription = manager.subscribe_events();
        manager.resize(&id, 50, 160)?;

        let envelope = wait_for_event(&subscription, Duration::from_secs(5), |envelope| {
            envelope.session_id == id && matches!(envelope.event, TerminalEvent::Resized { .. })
        });
        match envelope.map(|envelope| envelope.event) {
            Some(TerminalEvent::Resized { rows, cols }) => {
                assert_eq!((rows, cols), (50, 160));
            }
            other => panic!("expected a Resized{{50,160}} event, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn events_multiplex_across_sessions() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id_a = manager.open("t_multi_a", test_shell::spec())?;
        let id_b = manager.open("t_multi_b", test_shell::spec())?;
        // One global subscription watches both sessions at once — the core
        // "watch many consoles" guarantee.
        let subscription = manager.subscribe_events();
        manager.write(&id_a, &test_shell::echo("FROM_A"))?;
        manager.write(&id_b, &test_shell::echo("FROM_B"))?;

        let mut seen_a = false;
        let mut seen_b = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !(seen_a && seen_b) {
            if let Ok(envelope) = subscription
                .receiver
                .recv_timeout(Duration::from_millis(200))
                && matches!(envelope.event, TerminalEvent::OutputChunk { .. })
            {
                if envelope.session_id == id_a {
                    seen_a = true;
                } else if envelope.session_id == id_b {
                    seen_b = true;
                }
            }
        }
        assert!(
            seen_a,
            "expected an event tagged with session A on the shared stream"
        );
        assert!(
            seen_b,
            "expected an event tagged with session B on the shared stream"
        );
        Ok(())
    }

    #[test]
    fn shared_agents_write_and_human_preempts_exclusive_input() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let session_id = manager.open("t_multi_actor", test_shell::spec())?;
        let output = manager.subscribe(&session_id)?;
        let agent_a = InputActor {
            kind: InputActorKind::Agent,
            id: "agent-a".to_string(),
        };
        let agent_b = InputActor {
            kind: InputActorKind::Agent,
            id: "agent-b".to_string(),
        };
        let human = InputActor {
            kind: InputActorKind::Human,
            id: "human-a".to_string(),
        };

        let lease_a = manager.acquire_lease(
            &session_id,
            agent_a.clone(),
            InputLeaseMode::Shared,
            5_000,
            None,
        )?;
        let lease_b = manager.acquire_lease(
            &session_id,
            agent_b.clone(),
            InputLeaseMode::Shared,
            5_000,
            None,
        )?;
        manager.write_as(
            &session_id,
            agent_a.clone(),
            Some(&lease_a.lease_id),
            &test_shell::echo("ACTOR_A"),
        )?;
        assert!(
            wait_for(&output, "ACTOR_A", Duration::from_secs(5)),
            "first shared agent output was not written"
        );
        manager.write_as(
            &session_id,
            agent_b.clone(),
            Some(&lease_b.lease_id),
            &test_shell::echo("ACTOR_B"),
        )?;
        assert!(
            wait_for(&output, "ACTOR_B", Duration::from_secs(5)),
            "second shared agent output was not written"
        );

        let conflict = manager.acquire_lease(
            &session_id,
            agent_a.clone(),
            InputLeaseMode::Exclusive,
            5_000,
            Some(&lease_a.lease_id),
        );
        assert!(matches!(
            conflict,
            Err(TerminalError::InputLeaseConflict { .. })
        ));

        manager.release_lease(&session_id, &lease_b.lease_id)?;
        let exclusive = manager.acquire_lease(
            &session_id,
            agent_a.clone(),
            InputLeaseMode::Exclusive,
            5_000,
            Some(&lease_a.lease_id),
        )?;
        let events = manager.subscribe_events();

        manager.write_as(
            &session_id,
            human.clone(),
            None,
            &test_shell::echo("HUMAN_PREEMPTED"),
        )?;
        assert!(
            wait_for(&output, "HUMAN_PREEMPTED", Duration::from_secs(5)),
            "human input was not written after preemption"
        );
        assert!(matches!(
            manager.write_as(
                &session_id,
                agent_a.clone(),
                Some(&exclusive.lease_id),
                &test_shell::echo("SHOULD_NOT_RUN"),
            ),
            Err(TerminalError::InputLeaseNotFound { .. })
        ));
        assert!(manager.leases(&session_id)?.is_empty());

        let mut saw_preemption = false;
        let mut saw_human_input = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !(saw_preemption && saw_human_input) {
            if let Ok(envelope) = events.receiver.recv_timeout(Duration::from_millis(200)) {
                match envelope.event {
                    TerminalEvent::InputPreempted {
                        previous_actor,
                        by_actor,
                        lease_id,
                    } => {
                        assert_eq!(previous_actor, agent_a);
                        assert_eq!(by_actor, human);
                        assert_eq!(lease_id, exclusive.lease_id);
                        saw_preemption = true;
                    }
                    TerminalEvent::InputAccepted { actor, .. } if actor == human => {
                        saw_human_input = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_preemption, "expected human preemption event");
        assert!(saw_human_input, "expected accepted human input event");
        manager.close(&session_id)?;
        Ok(())
    }

    #[test]
    fn expired_agent_lease_rejects_input() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let session_id = manager.open("t_lease_expiry", test_shell::spec())?;
        let agent = InputActor {
            kind: InputActorKind::Agent,
            id: "expiring-agent".to_string(),
        };
        let lease = manager.acquire_lease(
            &session_id,
            agent.clone(),
            InputLeaseMode::Exclusive,
            20,
            None,
        )?;
        let events = manager.subscribe_events();
        std::thread::sleep(Duration::from_millis(40));

        assert!(matches!(
            manager.write_as(
                &session_id,
                agent.clone(),
                Some(&lease.lease_id),
                &test_shell::echo("EXPIRED"),
            ),
            Err(TerminalError::InputLeaseNotFound { .. })
        ));
        assert!(manager.leases(&session_id)?.is_empty());

        let mut saw_expired = false;
        let mut saw_rejected = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !(saw_expired && saw_rejected) {
            if let Ok(envelope) = events.receiver.recv_timeout(Duration::from_millis(200)) {
                match envelope.event {
                    TerminalEvent::InputLeaseReleased {
                        reason: InputLeaseReleaseReason::Expired,
                        ..
                    } => saw_expired = true,
                    TerminalEvent::InputRejected {
                        reason: InputRejectionReason::LeaseNotFound,
                        ..
                    } => saw_rejected = true,
                    _ => {}
                }
            }
        }
        assert!(saw_expired, "expected expired lease event");
        assert!(saw_rejected, "expected rejected input event");
        manager.close(&session_id)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn signals_reach_the_foreground_process_group() -> Result<(), TerminalError> {
        for (id, signal, trap_name, marker) in [
            (
                "t_signal_interrupt",
                TerminalSignal::Interrupt,
                "INT",
                "INTERRUPTED_BY_SIGNAL",
            ),
            (
                "t_signal_terminate",
                TerminalSignal::Terminate,
                "TERM",
                "TERMINATED_BY_SIGNAL",
            ),
        ] {
            let mut manager = TerminalSessionManager::new();
            let command = format!(
                "trap 'printf \"{marker}\\n\"; exit 0' {trap_name}; \
                 printf 'SIGNAL_READY\\n'; while :; do sleep 1; done"
            );
            let session_id = manager.open(
                id,
                TerminalOpenSpec {
                    program: Some("/bin/sh".to_string()),
                    args: vec!["-c".to_string(), command],
                    ..TerminalOpenSpec::default()
                },
            )?;
            let output = manager.subscribe(&session_id)?;
            assert!(
                wait_for(&output, "SIGNAL_READY", Duration::from_secs(5)),
                "signal test process did not become ready"
            );
            let events = manager.subscribe_events();

            manager.signal(&session_id, signal)?;

            assert!(
                wait_for(&output, marker, Duration::from_secs(5)),
                "{signal} did not reach the foreground process group"
            );
            let event = wait_for_event(&events, Duration::from_secs(5), |envelope| {
                envelope.session_id == session_id
                    && matches!(
                        envelope.event,
                        TerminalEvent::SignalSent {
                            signal: delivered,
                            ..
                        } if delivered == signal
                    )
            });
            assert!(event.is_some(), "expected a SignalSent event for {signal}");
            manager.close(&session_id)?;
        }

        let mut manager = TerminalSessionManager::new();
        let session_id = manager.open(
            "t_signal_kill",
            TerminalOpenSpec {
                program: Some("/bin/sh".to_string()),
                args: vec![
                    "-c".to_string(),
                    "printf 'SIGNAL_READY\\n'; while :; do sleep 1; done".to_string(),
                ],
                ..TerminalOpenSpec::default()
            },
        )?;
        let output = manager.subscribe(&session_id)?;
        assert!(
            wait_for(&output, "SIGNAL_READY", Duration::from_secs(5)),
            "kill test process did not become ready"
        );
        manager.signal(&session_id, TerminalSignal::Kill)?;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !matches!(
                manager.status(&session_id),
                Some(TerminalSessionStatus::Running)
            ) {
                manager.close(&session_id)?;
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("kill signal did not terminate the foreground process group");
    }

    #[test]
    fn secret_mode_withholds_output_and_suspends_agents() -> Result<(), TerminalError> {
        const SECRET: &str = "correct-horse-battery-staple";
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t_secret", test_shell::spec())?;
        let human = InputActor {
            kind: InputActorKind::Human,
            id: "human-a".to_string(),
        };
        let agent = InputActor {
            kind: InputActorKind::Agent,
            id: "agent-a".to_string(),
        };
        let lease =
            manager.acquire_lease(&id, agent.clone(), InputLeaseMode::Shared, 60_000, None)?;
        let output = manager.subscribe(&id)?;
        let events = manager.subscribe_events();

        // Before the window: this session publishes normally.
        manager.write_as(&id, human.clone(), None, &test_shell::echo("BEFORE_SECRET"))?;
        assert!(
            drain_until(&output, "BEFORE_SECRET", Duration::from_secs(5)).is_some(),
            "the session was not publishing before secret mode"
        );

        let status = manager.enter_secret(&id, human.clone(), "password prompt")?;
        assert!(status.active);
        assert_eq!(status.actor, Some(human.clone()));

        // Every non-human path into the session is refused while it is on.
        assert!(matches!(
            manager.write_as(&id, agent.clone(), Some(&lease.lease_id), b"whoami\n"),
            Err(TerminalError::SecretInputActive { .. })
        ));
        assert!(matches!(
            manager.signal_as(
                &id,
                agent.clone(),
                Some(&lease.lease_id),
                TerminalSignal::Interrupt
            ),
            Err(TerminalError::SecretInputActive { .. })
        ));
        assert!(matches!(
            manager.acquire_lease(&id, agent.clone(), InputLeaseMode::Shared, 5_000, None),
            Err(TerminalError::SecretInputActive { .. })
        ));
        // And an agent cannot simply switch it off again.
        assert!(matches!(
            manager.exit_secret(&id, agent.clone()),
            Err(TerminalError::SecretInputExitDenied { .. })
        ));

        // The person types the secret, and the shell echoes it right back.
        let secret_input = test_shell::echo(SECRET);
        manager.write_as(&id, human.clone(), None, &secret_input)?;

        // While the window is open the screen is withheld, not stale-looking.
        let screen = manager
            .screen(&id)
            .ok_or_else(|| TerminalError::NotFound(id.clone()))?;
        assert!(screen.secret_input);
        assert!(screen.lines.is_empty());
        assert!(!screen.cursor.visible);

        // Ending waits for the echo to stop arriving, so this is the same
        // retry a caller writes. It must succeed, not time out.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match manager.exit_secret(&id, human.clone()) {
                Ok(_) => break,
                Err(TerminalError::SecretInputSettling { .. }) => {
                    assert!(Instant::now() < deadline, "the session never settled");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        }
        manager.write_as(&id, human.clone(), None, &test_shell::echo("AFTER_SECRET"))?;

        // AFTER_SECRET is the barrier: once it arrives, everything the PTY
        // produced during the window has already been through the reader.
        let published = drain_until(&output, "AFTER_SECRET", Duration::from_secs(5))
            .ok_or_else(|| TerminalError::NotFound(id.clone()))?;
        assert!(
            !published.contains(SECRET),
            "the secret reached the raw byte stream: {published}"
        );

        // The protected grid is discarded before publication resumes.
        let screen = manager
            .screen(&id)
            .ok_or_else(|| TerminalError::NotFound(id.clone()))?;
        assert!(!screen.secret_input);
        assert!(
            !screen.lines.join("\n").contains(SECRET),
            "the secret reached the resumed screen: {:?}",
            screen.lines
        );

        // A fresh subscriber cannot replay it either: it was never retained.
        let replayed = String::from_utf8_lossy(&manager.subscribe(&id)?.backlog).into_owned();
        assert!(
            !replayed.contains(SECRET),
            "the secret was retained for replay: {replayed}"
        );

        // Barrier on the bus: the post-window write was announced, so every
        // event the window itself would have produced has already been fanned
        // out and is in `collected`.
        let mut window_closed = false;
        let collected =
            drain_events_until(
                &events,
                Duration::from_secs(5),
                |envelope| match &envelope.event {
                    TerminalEvent::SecretInputEnded { .. } => {
                        window_closed = true;
                        false
                    }
                    TerminalEvent::InputAccepted { .. } => window_closed,
                    _ => false,
                },
            )
            .ok_or_else(|| TerminalError::NotFound(id.clone()))?;
        let started = collected
            .iter()
            .position(|envelope| matches!(envelope.event, TerminalEvent::SecretInputStarted { .. }))
            .ok_or_else(|| TerminalError::NotFound(id.clone()))?;
        let ended = collected
            .iter()
            .position(|envelope| matches!(envelope.event, TerminalEvent::SecretInputEnded { .. }))
            .ok_or_else(|| TerminalError::NotFound(id.clone()))?;
        assert!(started < ended);
        for envelope in &collected[started + 1..ended] {
            match &envelope.event {
                TerminalEvent::InputRejected {
                    reason: InputRejectionReason::SecretInputActive,
                    ..
                } => {}
                other => panic!("secret window published {other:?}"),
            }
        }
        manager.close(&id)?;
        Ok(())
    }

    #[test]
    fn kill_emits_process_exited_event() -> Result<(), TerminalError> {
        let mut manager = TerminalSessionManager::new();
        let id = manager.open("t_kill_event", test_shell::spec())?;
        let subscription = manager.subscribe_events();
        manager.kill(&id)?;

        let envelope = wait_for_event(&subscription, Duration::from_secs(5), |envelope| {
            envelope.session_id == id
                && matches!(envelope.event, TerminalEvent::ProcessExited { .. })
        });
        assert!(
            envelope.is_some(),
            "expected a ProcessExited event after kill"
        );
        Ok(())
    }
}
