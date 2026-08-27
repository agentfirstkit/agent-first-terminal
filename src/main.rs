use std::io::Write;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use agent_first_data::skill::{
    self, SkillAction, SkillAgentSelection, SkillOptions, SkillScope, SkillSpec,
};
use agent_first_data::{
    ArgSpec, BoundOutcome, BuiltCliSpec, CliEmitter, CliSpec, CliSpecError, CliValue, Combination,
    CommandSpec, OutputFormat, OutputPlan, OutputSpec, OutputTo, ResolvedInvocation, SourceSet,
    build_afdata_cli, cli_error_event, cli_help_event, cli_parse_output, cli_version_event,
    json_error, json_progress, render_cli_reference,
};
use agent_first_ui::{Outcome, UiDeliveryMode, UiSession, UiUpstream};

/// The skill this binary installs. A single file: it sends agents to
/// `afterminal api --help` rather than shipping a reference tree to keep in step.
const SKILL_SPEC: SkillSpec = SkillSpec {
    name: "agent-first-terminal",
    source: include_str!("../skills/agent-first-terminal/SKILL.md"),
    title: "Agent-First Terminal",
    marker_slug: "afterminal",
    assets: &[],
};

/// The named agents, and the fan-out value that is not one of them.
const AGENTS: [&str; 4] = ["codex", "claude-code", "opencode", "hermes"];
const EVERY_AGENT: &str = "all";
use agent_first_terminal::api::{self, ApiState};
use agent_first_terminal::{
    MAX_TERMINAL_DIMENSION, MIN_TERMINAL_DIMENSION, TerminalOpenSpec, TerminalSessionManager,
};
use serde_json::{Value, json};

/// A server command keeps emitting events until it is stopped, so its data must
/// never land on the diagnostic stream: it is an event stream, and the contract
/// says so instead of a runtime check rejecting `--output-to split`.
fn stream_output() -> OutputSpec {
    OutputSpec::protocol_stream(
        ["json", "yaml", "plain"],
        ["stdout", "stderr"],
        "json",
        "stdout",
    )
}

fn finite_output() -> OutputSpec {
    OutputSpec::protocol_finite(
        ["json", "yaml", "plain"],
        ["split", "stdout", "stderr"],
        "json",
        "split",
    )
}

/// The sources the bearer credential accepts.
///
/// No stream sources: `afterminal` owns stdin for the terminal it runs, and a
/// prompt would block a server that is normally started by an agent.
fn access_token_sources() -> SourceSet {
    SourceSet::config()
}

fn access_token_arg() -> ArgSpec {
    ArgSpec::option("--access-token-secret", "SOURCE")
        .about("Bearer credential; falls back to AFTERMINAL_API_ACCESS_TOKEN_SECRET")
        .sources(access_token_sources())
}

/// Read the credential the argument named. AFDATA already refused an
/// unacceptable source while resolving argv, so this only fails on a source
/// that cannot be read — an unset variable, an unreadable file.
fn access_token(invocation: &ResolvedInvocation) -> Result<Option<String>, String> {
    let Some(raw) = optional_string(invocation, "access_token_secret") else {
        return Ok(None);
    };
    access_token_sources()
        .parse(&raw)
        .and_then(|source| source.read_secret())
        .map(|secret| Some(secret.expose_secret().to_string()))
        .map_err(|error| format!("--access-token-secret {error}"))
}

fn cli_spec() -> Result<BuiltCliSpec, CliSpecError> {
    build_afdata_cli(
        CliSpec::new("afterminal", env!("CARGO_PKG_VERSION"))
            .about("Run and expose the Agent-First Terminal PTY runtime.")
            .display_name("Agent-First Terminal")
            .lifecycle_output(finite_output())
            .command(CommandSpec::root())
            .command(
                CommandSpec::new(["api"])
                    .about("Serve or export the OpenAPI 3.2 terminal runtime contract."),
            )
            .command(
                CommandSpec::new(["api", "serve"])
                    .about("Serve the bearer-protected terminal API.")
                    .arg(
                        ArgSpec::option_i64("--port", "PORT")
                            .default_i64(9418)
                            .about("TCP port; use 0 to let the operating system choose"),
                    )
                    .arg(
                        ArgSpec::option_enum("--mode", ["local", "lan"])
                            .value_name("MODE")
                            .default("local")
                            .about(
                                "Network exposure: local binds 127.0.0.1; lan binds 0.0.0.0 and publishes the LAN address",
                            ),
                    )
                    .arg(access_token_arg())
                    .combination(
                        Combination::new("api-serve")
                            .action("api_serve")
                            .optional(["port", "mode", "access_token_secret"])
                            .output(stream_output()),
                    ),
            )
            .command(
                CommandSpec::new(["api", "export"])
                    .about("Write the generated OpenAPI document.")
                    .arg(
                        ArgSpec::option("--directory", "PATH")
                            .default("openapi")
                            .about("Destination directory"),
                    )
                    .arg(ArgSpec::flag("--force").about("Replace an existing generated document"))
                    .combination(
                        Combination::new("api-export")
                            .action("api_export")
                            .optional(["directory", "force"])
                            .output(finite_output()),
                    ),
            )
            .command(ui_command())
            .command(CommandSpec::new(["skill"]).about(
                "Manage the Agent-First Terminal skill for Codex, Claude Code, opencode, and Hermes.",
            ))
            .command(skill_command(
                "status",
                "Show whether the Agent-First Terminal skill is installed, valid, and up to date.",
                false,
            ))
            .command(skill_command(
                "install",
                "Install the Agent-First Terminal skill.",
                true,
            ))
            .command(skill_command(
                "uninstall",
                "Remove an afterminal-managed Agent-First Terminal skill.",
                true,
            )),
    )
}

/// `ui` in three shapes.
///
/// clap expressed this as one command plus a `conflicts_with = "api_url"` on
/// every session option and a runtime check that the session options require a
/// session id. Both are structural: attaching to a running API cannot configure
/// a session it did not create, and there is nothing for the session options to
/// configure without a session. As shapes, the parser rejects those mixes and
/// one `--help` shows all three.
fn ui_command() -> CommandSpec {
    CommandSpec::new(["ui"])
        .about("Deliver a UI over a new or already-running terminal runtime.")
        .arg(
            ArgSpec::positional("session_id", 0, "SESSION_ID")
                .about("Session to open, or to select when attaching through --api-url"),
        )
        .arg(
            ArgSpec::option("--api-url", "URL")
                .about("Existing terminal API to attach without creating a new runtime"),
        )
        .arg(
            ArgSpec::option_i64("--port", "PORT")
                .default_i64(0)
                .about("TCP port; use 0 to let the operating system choose"),
        )
        .arg(access_token_arg())
        .arg(
            ArgSpec::option("--program", "PROGRAM")
                .about("Program for the initial session. Defaults to the user's shell"),
        )
        .arg(
            ArgSpec::option("--arg", "ARG")
                .repeatable()
                .about("Argument passed directly to the initial program; may be repeated"),
        )
        .arg(
            ArgSpec::option("--cwd-path", "PATH")
                .about("Working directory for the initial session"),
        )
        .arg(
            ArgSpec::option_i64("--rows", "ROWS")
                .default_i64(24)
                .about("Initial terminal rows"),
        )
        .arg(
            ArgSpec::option_i64("--cols", "COLS")
                .default_i64(80)
                .about("Initial terminal columns"),
        )
        .arg(ArgSpec::option("--title", "TITLE").about("Advisory title for the initial session"))
        // Where the interface appears, as one explicit mode rather than a
        // negative switch — the same shape `api serve --mode` already has.
        .arg(UI_DELIVERY.arg("--mode"))
        .combination(
            Combination::new("ui-attach")
                .action("ui")
                .about("Attach to a running terminal API instead of starting one")
                .required(["api_url"])
                .optional(["session_id", "access_token_secret", "mode"])
                .output(stream_output()),
        )
        .combination(
            Combination::new("ui-serve")
                .action("ui")
                .about("Start and deliver a runtime with no initial terminal session")
                .optional(["port", "access_token_secret", "mode"])
                .output(stream_output()),
        )
        .combination(
            Combination::new("ui-serve-session")
                .action("ui")
                .about("Start and deliver a runtime with one configured terminal session")
                .required(["session_id"])
                .optional([
                    "port",
                    "access_token_secret",
                    "program",
                    "arg",
                    "cwd_path",
                    "rows",
                    "cols",
                    "title",
                    "mode",
                ])
                .output(stream_output()),
        )
}

fn main() -> ExitCode {
    let cli = match cli_spec() {
        Ok(cli) => cli,
        Err(error) => return emit_startup_error("cli_spec_invalid", &error.to_string()),
    };
    let app = match cli.bind_actions([
        (
            "api_serve",
            run_api_serve as fn(&ResolvedInvocation) -> ExitCode,
        ),
        ("api_export", run_api_export),
        ("ui", run_ui),
        ("skill_status", run_skill_status),
        ("skill_install", run_skill_install),
        ("skill_uninstall", run_skill_uninstall),
    ]) {
        Ok(app) => app,
        Err(error) => return emit_startup_error("cli_actions_invalid", &error.to_string()),
    };

    let outcome = match app.resolve_from(std::env::args_os()) {
        Ok(outcome) => outcome,
        Err(error) => {
            return emit_event(
                cli_error_event(&error),
                OutputFormat::Json,
                OutputTo::Stderr,
                error.exit_code(),
            );
        }
    };

    match outcome {
        BoundOutcome::Run(invocation) => invocation.run(),
        BoundOutcome::Docs(docs) => {
            write_text(&render_cli_reference(&cli), raw_stream(docs.output_plan()))
        }
        BoundOutcome::Help(help) => {
            let (format, output_to) = plan_output(help.output_plan());
            if format == OutputFormat::Plain {
                write_text(&help.plain(), raw_stream(help.output_plan()))
            } else {
                emit_event(cli_help_event(&help), format, output_to, 0)
            }
        }
        BoundOutcome::Version(version) => {
            let (format, output_to) = plan_output(version.output_plan());
            emit_event(cli_version_event(&version), format, output_to, 0)
        }
    }
}

fn plan_output(plan: &OutputPlan) -> (OutputFormat, OutputTo) {
    let format = plan
        .format()
        .and_then(|format| cli_parse_output(format).ok())
        .unwrap_or(OutputFormat::Json);
    let output_to = plan
        .destination()
        .and_then(|destination| OutputTo::parse(destination).ok())
        .unwrap_or(OutputTo::Split);
    (format, output_to)
}

fn raw_stream(plan: &OutputPlan) -> OutputTo {
    if plan.destination() == Some("stderr") {
        OutputTo::Stderr
    } else {
        OutputTo::Stdout
    }
}

/// One skill verb, in the two shapes that partition `--agent`.
///
/// `--skills-dir` names a single directory, so it is legal only once the call
/// has named one agent; the fan-out shape would have to write one path for all.
fn skill_command(verb: &str, about: &str, force: bool) -> CommandSpec {
    let mut command = CommandSpec::new(["skill", verb])
        .about(about)
        .arg(
            ArgSpec::option_enum("--agent", std::iter::once(EVERY_AGENT).chain(AGENTS))
                .value_name("AGENT")
                .default(EVERY_AGENT)
                .about("Agent to manage"),
        )
        .arg(
            ArgSpec::option_enum("--scope", ["personal", "workspace"])
                .value_name("SCOPE")
                .default("personal")
                .about("Skill scope"),
        )
        .arg(ArgSpec::option("--skills-dir", "DIR").about("Directory that contains skill folders"));

    let mut every: Vec<&str> = vec!["scope"];
    let mut named: Vec<&str> = vec!["scope", "skills_dir"];
    if force {
        command = command.arg(ArgSpec::flag("--force").about(
            "Overwrite or remove an unmanaged Agent-First Terminal skill at the target path",
        ));
        every.push("force");
        named.push("force");
    }

    command
        .combination(
            Combination::new(format!("skill-{verb}-every-agent"))
                .action(format!("skill_{verb}"))
                .about("Target every agent that supports the scope")
                .fixed("agent", EVERY_AGENT)
                .optional(every)
                .output(finite_output()),
        )
        .combination(
            Combination::new(format!("skill-{verb}-one-agent"))
                .action(format!("skill_{verb}"))
                .about("Target one named agent; only this shape accepts --skills-dir")
                .fixed_one_of("agent", AGENTS)
                .optional(named)
                .output(finite_output()),
        )
}

fn run_skill_status(invocation: &ResolvedInvocation) -> ExitCode {
    run_skill(invocation, SkillAction::Status)
}

fn run_skill_install(invocation: &ResolvedInvocation) -> ExitCode {
    run_skill(invocation, SkillAction::Install)
}

fn run_skill_uninstall(invocation: &ResolvedInvocation) -> ExitCode {
    run_skill(invocation, SkillAction::Uninstall)
}

fn run_skill(invocation: &ResolvedInvocation, action: SkillAction) -> ExitCode {
    let (format, output_to) = plan_output(invocation.output_plan());
    let options = SkillOptions {
        agent: match optional_string(invocation, "agent").as_deref() {
            Some("codex") => SkillAgentSelection::Codex,
            Some("claude-code") => SkillAgentSelection::ClaudeCode,
            Some("opencode") => SkillAgentSelection::Opencode,
            Some("hermes") => SkillAgentSelection::Hermes,
            _ => SkillAgentSelection::All,
        },
        scope: match optional_string(invocation, "scope").as_deref() {
            Some("workspace") => SkillScope::Workspace,
            _ => SkillScope::Personal,
        },
        skills_dir: optional_string(invocation, "skills_dir"),
        force: invocation
            .optional("force")
            .and_then(CliValue::as_bool)
            .unwrap_or(false),
    };
    match skill::run_skill_admin(&SKILL_SPEC, action, &options) {
        Ok(report) => match serde_json::to_value(&report) {
            Ok(value) => emit_event(
                agent_first_data::json_result(value).build(),
                format,
                output_to,
                0,
            ),
            Err(error) => emit_domain_error(
                "serialization_failed",
                &format!("failed to serialize skill report: {error}"),
                format,
                output_to,
            ),
        },
        Err(err) => emit_domain_error("skill_error", &err.message, format, output_to),
    }
}

fn emit_domain_error(
    code: &str,
    message: &str,
    format: OutputFormat,
    output_to: OutputTo,
) -> ExitCode {
    match json_error(code, message).build() {
        Ok(event) => emit_event(event, format, output_to, 1),
        Err(_) => ExitCode::from(4),
    }
}

fn optional_string(invocation: &ResolvedInvocation, id: &str) -> Option<String> {
    invocation
        .optional(id)
        .and_then(CliValue::as_str)
        .map(str::to_string)
}

fn run_api_export(invocation: &ResolvedInvocation) -> ExitCode {
    let (format, output_to) = plan_output(invocation.output_plan());
    let directory = optional_string(invocation, "directory").unwrap_or_default();
    let force = invocation
        .optional("force")
        .and_then(CliValue::as_bool)
        .unwrap_or(false);
    run_export(Path::new(&directory), force, format, output_to)
}

fn run_api_serve(invocation: &ResolvedInvocation) -> ExitCode {
    let (format, output_to) = plan_output(invocation.output_plan());
    let port = match bounded_u16(invocation, "port") {
        Ok(port) => port,
        Err(code) => return code,
    };
    let network = match optional_string(invocation, "mode").as_deref() {
        Some("lan") => NetworkMode::Lan,
        _ => NetworkMode::Local,
    };
    let token = match access_token(invocation) {
        Ok(token) => token,
        Err(message) => return invalid_value(&message, format, output_to),
    };
    run_server(
        ServerMode::Api { network },
        port,
        token.as_deref(),
        format,
        output_to,
    )
}

fn run_ui(invocation: &ResolvedInvocation) -> ExitCode {
    let (format, output_to) = plan_output(invocation.output_plan());
    let token = match access_token(invocation) {
        Ok(token) => token,
        Err(message) => return invalid_value(&message, format, output_to),
    };
    let session_id = match optional_string(invocation, "session_id") {
        Some(raw) => match parse_session_id(&raw) {
            Ok(session_id) => Some(session_id),
            Err(message) => return invalid_value(&message, format, output_to),
        },
        None => None,
    };
    let mode = match delivery_mode_of(invocation).and_then(resolve_delivery_mode) {
        Ok(mode) => mode,
        Err(message) => {
            return emit_domain_error("ui_delivery_unavailable", &message, format, output_to);
        }
    };

    if let Some(api_url) = optional_string(invocation, "api_url") {
        return run_attach_ui(
            &api_url,
            session_id.as_deref(),
            token.as_deref(),
            mode,
            format,
            output_to,
        );
    }

    let port = match bounded_u16(invocation, "port") {
        Ok(port) => port,
        Err(code) => return code,
    };
    let initial_session = match session_id {
        None => None,
        Some(session_id) => {
            let rows = match dimension(invocation, "rows", format, output_to) {
                Ok(rows) => rows,
                Err(code) => return code,
            };
            let cols = match dimension(invocation, "cols", format, output_to) {
                Ok(cols) => cols,
                Err(code) => return code,
            };
            Some(InitialSession {
                session_id,
                spec: TerminalOpenSpec {
                    program: optional_string(invocation, "program"),
                    args: invocation
                        .repeated("arg")
                        .iter()
                        .filter_map(CliValue::as_str)
                        .map(str::to_string)
                        .collect(),
                    cwd: optional_string(invocation, "cwd_path").map(PathBuf::from),
                    rows,
                    cols,
                    title: optional_string(invocation, "title"),
                    ..TerminalOpenSpec::default()
                },
            })
        }
    };

    run_server(
        ServerMode::Ui {
            initial_session,
            mode,
        },
        port,
        token.as_deref(),
        format,
        output_to,
    )
}

/// The registry types these as i64; the domain range is this binary's to state.
fn bounded_u16(invocation: &ResolvedInvocation, id: &str) -> Result<u16, ExitCode> {
    let (format, output_to) = plan_output(invocation.output_plan());
    match invocation.optional(id).and_then(CliValue::as_i64) {
        Some(value) if (0..=i64::from(u16::MAX)).contains(&value) => Ok(value as u16),
        Some(_) => Err(invalid_value(
            &format!("--{} must be between 0 and 65535", id.replace('_', "-")),
            format,
            output_to,
        )),
        None => Ok(0),
    }
}

fn dimension(
    invocation: &ResolvedInvocation,
    id: &str,
    format: OutputFormat,
    output_to: OutputTo,
) -> Result<u16, ExitCode> {
    match invocation.optional(id).and_then(CliValue::as_i64) {
        Some(value)
            if (i64::from(MIN_TERMINAL_DIMENSION)..=i64::from(MAX_TERMINAL_DIMENSION))
                .contains(&value) =>
        {
            Ok(value as u16)
        }
        _ => Err(invalid_value(
            &format!(
                "--{id} must be between {MIN_TERMINAL_DIMENSION} and \
                 {MAX_TERMINAL_DIMENSION}"
            ),
            format,
            output_to,
        )),
    }
}

/// The registry cannot type a session id or a bounded dimension, so this binary
/// checks them — and reports the classification the parser would have used,
/// rather than a second spelling for the same kind of failure.
fn invalid_value(message: &str, format: OutputFormat, output_to: OutputTo) -> ExitCode {
    let event = match json_error("cli_invalid_argument_value", message)
        .hint("run `afterminal --help` and choose one registered combination")
        .build()
    {
        Ok(event) => event,
        Err(_) => return ExitCode::from(4),
    };
    emit_event(event, format, output_to, 2)
}

fn emit_event(
    event: agent_first_data::Event,
    format: OutputFormat,
    output_to: OutputTo,
    exit_code: u8,
) -> ExitCode {
    let mut emitter = CliEmitter::from_output_to(output_to, format).with_strict_protocol();
    match emitter.emit(event) {
        Ok(()) => ExitCode::from(exit_code),
        Err(_) => ExitCode::from(4),
    }
}

fn emit_startup_error(code: &str, message: &str) -> ExitCode {
    let event = match json_error(code, message).build() {
        Ok(event) => event,
        Err(_) => return ExitCode::from(4),
    };
    emit_event(event, OutputFormat::Json, OutputTo::Stderr, 1)
}

fn run_export(
    directory: &Path,
    force: bool,
    output: OutputFormat,
    output_to: OutputTo,
) -> ExitCode {
    let mut emitter = CliEmitter::from_output_to(output_to, output).with_strict_protocol();
    match api::export_contract(directory, force) {
        Ok(summary) => ExitCode::from(emitter.finish_result(json!({
            "openapi_path": summary.openapi_path.to_string_lossy().replace('\\', "/"),
            "schema_index_path": summary.schema_index_path.to_string_lossy().replace('\\', "/"),
            "schema_directory_path": summary
                .schema_directory_path
                .to_string_lossy()
                .replace('\\', "/"),
            "schema_count": summary.schema_count,
            "file_count": summary.file_count,
        }))),
        Err(error) => finish_error(&mut emitter, "openapi_export_failed", &error.to_string(), 1),
    }
}

struct InitialSession {
    session_id: String,
    spec: TerminalOpenSpec,
}

/// Where the API listens, as one explicit choice rather than a pile of
/// overlapping switches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetworkMode {
    Local,
    Lan,
}

impl NetworkMode {
    fn as_word(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Lan => "lan",
        }
    }

    fn bind_ip(self) -> Ipv4Addr {
        match self {
            Self::Local => Ipv4Addr::LOCALHOST,
            Self::Lan => Ipv4Addr::UNSPECIFIED,
        }
    }

    /// The address to publish. On a LAN that is this machine's own address, so
    /// the person reading the ready event has a URL that works from a phone.
    fn advertised_ip(self) -> Result<Ipv4Addr, ServeError> {
        match self {
            Self::Local => Ok(Ipv4Addr::LOCALHOST),
            // The same probe AFUI advertises a `link` session with, taken
            // from AFUI rather than copied: why it probes the local multicast
            // route instead of the default one is a property of how VPN and
            // overlay interfaces behave, not of what afterminal is publishing.
            Self::Lan => agent_first_ui::primary_lan_ipv4().map_err(|error| {
                ServeError::Runtime(format!(
                    "find this machine's LAN IPv4 address: {error}; connect to a trusted IPv4 network or use --mode local"
                ))
            }),
        }
    }

    fn ready_message(self) -> &'static str {
        match self {
            Self::Local => "The afterminal API is available on this machine.",
            Self::Lan => {
                "The afterminal API is on the trusted local network; keep the bearer credential private."
            }
        }
    }
}

enum ServerMode {
    Api {
        network: NetworkMode,
    },
    Ui {
        initial_session: Option<InitialSession>,
        mode: UiDeliveryMode,
    },
}

/// Which deliveries the terminal UI offers, declared once: the flag above and
/// the plan in `run_server` are built from this same value.
const UI_DELIVERY: agent_first_ui::cli::UiDeliveryOffer =
    agent_first_ui::cli::UiDeliveryOffer::WithLink;

fn delivery_mode_of(invocation: &ResolvedInvocation) -> Result<Option<UiDeliveryMode>, String> {
    agent_first_ui::cli::delivery_of(invocation, "mode").map_err(|error| error.to_string())
}

fn resolve_delivery_mode(explicit: Option<UiDeliveryMode>) -> Result<UiDeliveryMode, String> {
    UiDeliveryMode::resolve(explicit).map_err(|error| error.to_string())
}

fn child_afui_delivery(mode: UiDeliveryMode) -> Option<&'static str> {
    (mode != UiDeliveryMode::Window).then_some("session")
}

/// What only afterminal knows about reaching a terminal this way.
///
/// Where the UI is comes from AFUI. What it costs to publish *this* UI there
/// does not: a terminal link is control of every session in the runtime, which
/// is a stronger statement than "reachable on the network" and is afterminal's
/// to make.
fn link_caveat(mode: UiDeliveryMode) -> &'static str {
    match mode {
        UiDeliveryMode::Link => {
            " That URL is a bearer capability with control of every terminal session in this \
             runtime; share it only through a trusted channel and do not log or persist it."
        }
        UiDeliveryMode::Window | UiDeliveryMode::Session => "",
    }
}

/// One readiness event: what AFUI knows about the delivery, spliced together
/// with what afterminal knows about the runtime behind it.
///
/// One field is renamed on the way through. AFUI names the bearer URL
/// `link_url_secret`, the right name for a value nothing should log — and the
/// wrong one for the single event whose job is to hand that URL to the person
/// who asked for `link`, since a structured-output layer would redact it out of
/// the handoff. `link_url` still gets URL-component redaction. The rename is
/// deliberate and belongs here, because only afterminal knows this event *is*
/// the handoff.
/// The readiness event, with the link URL under the name that survives an
/// emitter — the rename this used to do by hand is AFUI's now, and so is the
/// reason for it.
fn ui_ready_event(facts: &agent_first_ui::UiDeliveryFacts, event: Value) -> Value {
    agent_first_ui::cli::ready_event_revealing_link(facts, event)
}

fn run_server(
    mode: ServerMode,
    port: u16,
    explicit_token: Option<&str>,
    output: OutputFormat,
    output_to: OutputTo,
) -> ExitCode {
    let started = Instant::now();
    let ui_enabled = matches!(&mode, ServerMode::Ui { .. });
    let mut emitter = CliEmitter::from_output_to(output_to, output).with_strict_protocol();
    let token = match api::resolve_access_token(explicit_token) {
        Ok(token) => token,
        Err(error) => {
            return finish_error(
                &mut emitter,
                "api_access_token_invalid",
                &error.to_string(),
                1,
            );
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return finish_error(&mut emitter, "api_runtime_failed", &error.to_string(), 1);
        }
    };
    // The page is resolved, rendered and checked before a listener is bound, so
    // a frontend afterminal cannot use costs a person an error rather than a
    // window with nothing in it — and never a quietly substituted built-in page.
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let terminal_ui = match api::TerminalUi::resolve(&workspace_root) {
        Ok(terminal_ui) => Arc::new(terminal_ui),
        Err(message) => {
            return finish_error(&mut emitter, "ui_frontend_unusable", &message, 1);
        }
    };
    let outcome = runtime.block_on(async {
        let network = match &mode {
            ServerMode::Api { network } => *network,
            // A window is served to this machine's own browser, so the runtime
            // behind it stays on loopback.
            ServerMode::Ui { .. } => NetworkMode::Local,
        };
        let advertised_ip = network.advertised_ip()?;
        let listener = tokio::net::TcpListener::bind((network.bind_ip(), port))
            .await
            .map_err(|error| ServeError::Runtime(format!("bind afterminal API: {error}")))?;
        let address = listener
            .local_addr()
            .map_err(|error| ServeError::Runtime(format!("read API address: {error}")))?;
        let api_url = format!("http://{advertised_ip}:{}", address.port());
        match mode {
            ServerMode::Api { network } => {
                emitter
                    .emit(
                        json_progress(json!({
                            "phase": "api_ready",
                            "message": network.ready_message(),
                            "api_url": api_url,
                            "openapi_url": format!("{api_url}/openapi.json"),
                            "schema_index_url": format!("{api_url}/schemas/index.json"),
                            "mode": network.as_word(),
                            "port": address.port(),
                        }))
                        .trace(json!({"duration_ms": elapsed_ms(started)}))
                        .build(),
                    )
                    .map_err(|_| ServeError::Output)?;
                let state = ApiState::new(token);
                // The same URL this API just advertised: it is the name it will
                // answer UI requests under, and AFUI refuses any other.
                let app = api::router(state, Arc::clone(&terminal_ui), &api_url)
                    .map_err(|error| ServeError::Runtime(format!("mount terminal UI: {error}")))?;
                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown_signal())
                    .await
                    .map_err(|error| {
                        ServeError::Runtime(format!("serve afterminal API: {error}"))
                    })?;
                Ok((api_url, false, None))
            }
            ServerMode::Ui {
                initial_session,
                mode,
            } => {
                let mut manager = TerminalSessionManager::new();
                // afterminal is the one hop in "follow afui" that has this
                // information: only it knows whether *this* terminal reaches
                // a person at this machine or not, and every PTY child it
                // opens (the initial session and any opened later through the
                // API) should carry that fact down to a sub-command with a UI
                // of its own. `window` sets nothing — forcing a value would
                // stomp whatever a person already exported in their own
                // shell. Both remote deliveries map to `session`: a
                // sub-command should register into the registry this terminal
                // was already reached through rather than bind a second LAN
                // listener of its own.
                if let Some(delivery) = child_afui_delivery(mode) {
                    manager = manager.with_afui_delivery(delivery);
                }
                // Computed before `initial_session` is consumed below.
                let subject = ui_session_subject(initial_session.as_ref());
                let initial_session_id = if let Some(initial_session) = initial_session {
                    let session_id = initial_session.session_id;
                    manager
                        .open(session_id.clone(), initial_session.spec)
                        .map_err(|error| {
                            ServeError::Runtime(format!("open initial terminal session: {error}"))
                        })?;
                    Some(session_id)
                } else {
                    None
                };
                let state = ApiState::with_manager(manager, token);
                // Two listeners over one runtime. `--port` stays the API: it is
                // the address an external controller was told to use, and it is
                // bearer-protected. AFUI binds its own loopback port for the
                // window, so the browser reaches the UI without ever being on a
                // listener that would accept the API bearer.
                let (stop_api, api_stopped) = tokio::sync::oneshot::channel::<()>();
                let app = api::router(state.clone(), Arc::clone(&terminal_ui), &api_url)
                    .map_err(|error| ServeError::Runtime(format!("mount terminal UI: {error}")))?;
                let api = axum::serve(listener, app).with_graceful_shutdown(async move {
                    let _stopped = api_stopped.await;
                });
                let api_task = tokio::spawn(async move { api.await });

                let session = UiSession::<()>::new(api::UI_PROVIDER_ID, api::UI_KIND)
                    .map_err(|error| ServeError::Runtime(error.to_string()))?
                    .with_app_icon(terminal_ui.app_icon())
                    .with_security_policy(api::ui_security_policy())
                    .with_subject(subject);
                // Ctrl-C ends the session the same way closing the window does,
                // so the one waiter below covers both.
                let interrupted = session.completion();
                tokio::spawn(async move {
                    shutdown_signal().await;
                    interrupted.complete(()).await;
                });
                let (session, ui_runtime) = api::attach_ui_runtime(session)
                    .map_err(|error| ServeError::Runtime(error.to_string()))?;
                api::publish_ui_state(&state, &ui_runtime)
                    .map_err(|error| ServeError::Runtime(error.to_string()))?;
                let ui_runtime_task = tokio::spawn(api::run_ui_runtime(state, ui_runtime));
                let router = api::ui_router(Arc::clone(&terminal_ui));
                let active = UI_DELIVERY
                    .resolve(Some(mode))
                    .map_err(|error| ServeError::Runtime(error.to_string()))?
                    .start(session, router)
                    .await
                    .map_err(|error| ServeError::Runtime(error.to_string()))?;
                emitter
                    .emit(
                        json_progress(ui_ready_event(
                            &active.facts(),
                            json!({
                                "phase": "ui_ready",
                                "message": format!(
                                    "The terminal runtime is ready: {}.{}",
                                    mode.description(),
                                    link_caveat(mode),
                                ),
                                "api_url": api_url,
                                "openapi_url": format!("{api_url}/openapi.json"),
                                "schema_index_url": format!("{api_url}/schemas/index.json"),
                                // The listener `--port` names, which for either
                                // UI mode is always loopback: the browser
                                // reaches the page through AFUI's own port,
                                // never this one.
                                "api_mode": "local",
                                "port": address.port(),
                                "initial_session_id": initial_session_id,
                                // Absent when afterminal's own page is serving.
                                // An untrusted workspace frontend is skipped in
                                // silence, so this is how an agent tells "my
                                // override is running" from "my override is
                                // inert".
                                "ui_frontend_id": terminal_ui.frontend_id(),
                            }),
                        ))
                        .trace(json!({"duration_ms": elapsed_ms(started)}))
                        .build(),
                    )
                    .map_err(|_| ServeError::Output)?;
                let outcome = active
                    .wait()
                    .await
                    .map_err(|error| ServeError::Runtime(error.to_string()))?;
                let _runtime_stopped = ui_runtime_task.await;
                let _stopping = stop_api.send(());
                match api_task.await {
                    Ok(result) => result.map_err(|error| {
                        ServeError::Runtime(format!("serve afterminal API: {error}"))
                    })?,
                    Err(error) => {
                        return Err(ServeError::Runtime(format!("stop afterminal API: {error}")));
                    }
                }
                Ok((api_url, true, Some(outcome)))
            }
        }
    });
    match outcome {
        Ok((api_url, ui, ending)) => ExitCode::from(emitter.finish_result(json!({
            "api_url": api_url,
            "ui_enabled": ui,
            "stopped": true,
            "ending": ending.as_ref().map(Outcome::ending),
        }))),
        Err(ServeError::Runtime(message)) => finish_error(
            &mut emitter,
            if ui_enabled {
                "ui_failed"
            } else {
                "api_serve_failed"
            },
            &message,
            1,
        ),
        Err(ServeError::Output) => ExitCode::from(4),
    }
}

fn run_attach_ui(
    api_url: &str,
    initial_session_id: Option<&str>,
    explicit_token: Option<&str>,
    mode: UiDeliveryMode,
    output: OutputFormat,
    output_to: OutputTo,
) -> ExitCode {
    let started = Instant::now();
    let mut emitter = CliEmitter::from_output_to(output_to, output).with_strict_protocol();
    let api_access_token_secret = match api::resolve_access_token(explicit_token) {
        Ok(token) => token,
        Err(error) => {
            return finish_error(
                &mut emitter,
                "api_access_token_invalid",
                &error.to_string(),
                1,
            );
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return finish_error(&mut emitter, "api_runtime_failed", &error.to_string(), 1);
        }
    };
    // No frontend is resolved here. `ui --api-url` attaches to a runtime
    // somewhere else: that machine renders and serves the page, so its
    // workspace decides which frontend is in force. Resolving one locally would
    // read a directory nobody is serving from.
    let outcome = runtime.block_on(async {
        let attachment =
            api::create_remote_ui_attachment(api_url, &api_access_token_secret, initial_session_id)
                .await
                .map_err(|error| ServeError::Runtime(error.to_string()))?;
        let normalized_api_url = attachment.api_url();
        let browser_url = attachment
            .browser_url(initial_session_id)
            .map_err(|error| ServeError::Runtime(error.to_string()))?;

        // The remote API serves the page and owns its private credential; AFUI
        // owns where that same upstream appears. Its delivery plan is shared
        // with an in-process Router, including the Link attention policy and a
        // registered Session's lack of a delivery clock.
        let upstream = UiUpstream::new(api::UI_PROVIDER_ID, api::UI_KIND, &browser_url)
            .map_err(|error| ServeError::Runtime(error.to_string()))?
            .with_subject(attach_subject(&normalized_api_url, initial_session_id));
        let active = match UI_DELIVERY
            .resolve(Some(mode))
            .map_err(|error| ServeError::Runtime(error.to_string()))?
            .start_upstream(upstream)
            .await
        {
            Ok(active) => active,
            Err(error) => {
                let _revoked = attachment.revoke().await;
                return Err(ServeError::Runtime(error.to_string()));
            }
        };
        if emitter
            .emit(
                json_progress(ui_ready_event(
                    &active.facts(),
                    json!({
                        "phase": "ui_ready",
                        "message": format!(
                            "The existing terminal runtime is ready: {}.{}",
                            mode.description(),
                            link_caveat(mode),
                        ),
                        "api_url": normalized_api_url,
                        "initial_session_id": initial_session_id,
                    }),
                ))
                .trace(json!({"duration_ms": elapsed_ms(started)}))
                .build(),
            )
            .is_err()
        {
            let _revoked = attachment.revoke().await;
            return Err(ServeError::Output);
        }
        let ending = {
            let delivery = active.wait();
            let keep_alive = attachment.keep_alive();
            tokio::pin!(delivery);
            tokio::pin!(keep_alive);
            tokio::select! {
                outcome = &mut delivery => outcome
                    .map_err(|error| ServeError::Runtime(error.to_string()))?
                    .ending(),
                () = shutdown_signal() => "stopped",
                maintained = &mut keep_alive => match maintained {
                    Err(error) => return Err(ServeError::Runtime(error.to_string())),
                    Ok(()) => return Err(ServeError::Runtime(
                        "terminal UI attachment keep-alive ended unexpectedly".to_string(),
                    )),
                },
            }
        };
        // Dropping the AFUI delivery closes its window or remote page and
        // withdraws its listing. The remote capability is revoked either way;
        // its idle timeout only cleans up a process that dies before this call.
        let ui_capability_revoked = attachment.revoke().await.unwrap_or(false);
        Ok((normalized_api_url, ui_capability_revoked, ending))
    });
    match outcome {
        Ok((normalized_api_url, ui_capability_revoked, ending)) => {
            ExitCode::from(emitter.finish_result(json!({
                "api_url": normalized_api_url,
                "ui_attached": true,
                "ui_capability_revoked": ui_capability_revoked,
                "mode": mode.as_str(),
                "ending": ending,
                "stopped": true,
            })))
        }
        Err(ServeError::Runtime(message)) => {
            finish_error(&mut emitter, "ui_attach_failed", &message, 1)
        }
        Err(ServeError::Output) => ExitCode::from(4),
    }
}

/// What `afui session list` calls a runtime this command started itself.
///
/// Identity rather than presentation: without it, every `afterminal ui`
/// window looks the same in the list and in the shell, because AFUI never
/// reads what is inside the terminal. `--title` is the material a person
/// deliberately gave for exactly this purpose, so it wins outright; absent
/// one, the initial program and working directory are what a person actually
/// recognizes an unlabeled terminal by.
fn ui_session_subject(initial_session: Option<&InitialSession>) -> String {
    let spec = initial_session.map(|session| &session.spec);
    if let Some(title) = spec.and_then(|spec| spec.title.as_deref()) {
        return title.to_string();
    }
    let program = spec
        .and_then(|spec| spec.program.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));
    let cwd = spec
        .and_then(|spec| spec.cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    match cwd {
        Some(cwd) => format!("{program} · {}", cwd.to_string_lossy().replace('\\', "/")),
        None => program,
    }
}

/// What `afui session list` calls an attached runtime.
///
/// Identity rather than presentation, and credential-free: the machine the
/// runtime is on is what tells two attached deliveries apart, and the session
/// id narrows it when one was named. The credential lives in the upstream URL,
/// which AFUI stores `0600` and never prints — it must not also arrive here,
/// where it would be shown to anyone listing sessions.
fn attach_subject(api_url: &str, initial_session_id: Option<&str>) -> String {
    match initial_session_id {
        Some(session_id) => format!("{api_url} · {session_id}"),
        None => api_url.to_string(),
    }
}

enum ServeError {
    Runtime(String),
    Output,
}

async fn shutdown_signal() {
    let _result = tokio::signal::ctrl_c().await;
}

fn finish_error(
    emitter: &mut CliEmitter<Box<dyn Write>>,
    code: &str,
    message: &str,
    exit_code: u8,
) -> ExitCode {
    match emitter.emit_error(code, message) {
        Ok(()) => ExitCode::from(exit_code),
        Err(_) => ExitCode::from(4),
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn parse_session_id(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 128 {
        return Err("session id must contain 1-128 ASCII characters".to_string());
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err("session id must not be empty".to_string());
    };
    if !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(
            "session id must start with an ASCII letter or digit and contain only letters, digits, dot, underscore, or hyphen"
                .to_string(),
        );
    }
    Ok(value.to_string())
}

// AFDATA injects the raw outcomes this writes (`--docs`, plain help), so it owns
// the routing and the rule that a closed reader is success rather than failure.
fn write_text(text: &str, output_to: OutputTo) -> ExitCode {
    match agent_first_data::write_raw(text, output_to) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(4),
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use agent_first_data::CliOutcome;

    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn spec() -> BuiltCliSpec {
        match cli_spec() {
            Ok(cli) => cli,
            Err(error) => panic!("registry must build: {error}"),
        }
    }

    #[test]
    fn every_shape_is_reachable() {
        let cli = spec();
        let synthetics = cli.synthetic_invocations();
        assert!(!synthetics.is_empty(), "the registry generated no fixtures");
        for synthetic in synthetics {
            let argv = synthetic.argv.clone();
            match cli.resolve_from(argv.clone()) {
                Ok(CliOutcome::Run(invocation)) => assert_eq!(
                    invocation.combination_id(),
                    synthetic.combination_id,
                    "{argv:?} resolved to the wrong shape"
                ),
                Ok(_) => panic!("{argv:?} did not resolve to a run"),
                Err(error) => panic!("{argv:?} failed to resolve: {}", error.message),
            }
        }
    }

    #[test]
    fn attaching_cannot_configure_a_session_it_did_not_create() {
        let cli = spec();
        let error = match cli.resolve_from([
            "afterminal",
            "ui",
            "--api-url",
            "http://127.0.0.1:9418",
            "--program",
            "codex",
        ]) {
            Err(error) => error,
            Ok(_) => panic!("attach must not accept session-creation options"),
        };
        assert_eq!(
            error.rule,
            agent_first_data::CliErrorRule::UnregisteredCombination
        );
    }

    #[test]
    fn attaching_uses_the_same_delivery_modes_as_a_new_runtime() {
        let cli = spec();
        let outcome = cli.resolve_from([
            "afterminal",
            "ui",
            "--api-url",
            "http://127.0.0.1:9418",
            "--mode",
            "session",
        ]);
        let Ok(CliOutcome::Run(invocation)) = outcome else {
            panic!("attach must accept AFUI delivery mode");
        };
        assert_eq!(invocation.combination_id(), "ui-attach");
        assert_eq!(
            delivery_mode_of(&invocation),
            Ok(Some(UiDeliveryMode::Session))
        );
    }

    #[test]
    fn session_options_without_a_session_are_rejected() {
        let cli = spec();
        assert!(
            cli.resolve_from(["afterminal", "ui", "--program", "codex"])
                .is_err(),
            "the session options have nothing to configure without a session id"
        );
        // The same options with a session id are the registered shape.
        let outcome = cli.resolve_from(["afterminal", "ui", "codex", "--program", "bash"]);
        let Ok(CliOutcome::Run(invocation)) = outcome else {
            panic!("a named session with session options must resolve");
        };
        assert_eq!(invocation.combination_id(), "ui-serve-session");
    }

    #[test]
    fn a_server_command_refuses_the_diagnostic_split() {
        let cli = spec();
        // Declared as an event stream, so the split that would strand its
        // events on stderr is not in its contract at all.
        assert!(
            cli.resolve_from(["afterminal", "api", "serve", "--output-to", "split"])
                .is_err(),
            "a stream command must reject --output-to split"
        );
        assert!(
            cli.resolve_from(["afterminal", "api", "export", "--output-to", "split"])
                .is_ok(),
            "a finite command still accepts the split default"
        );
    }

    #[test]
    fn ui_delivery_has_no_cli_default_and_accepts_link() {
        let cli = spec();
        let Ok(CliOutcome::Run(implicit)) = cli.resolve_from(["afterminal", "ui"]) else {
            panic!("the implicit UI shape must resolve");
        };
        assert_eq!(delivery_mode_of(&implicit), Ok(None));

        let Ok(CliOutcome::Run(link)) = cli.resolve_from(["afterminal", "ui", "--mode", "link"])
        else {
            panic!("link delivery must resolve");
        };
        assert_eq!(delivery_mode_of(&link), Ok(Some(UiDeliveryMode::Link)));
    }

    #[test]
    fn delivery_follows_the_environment_but_an_explicit_mode_wins() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        // SAFETY: every test here that mutates this process-global variable is
        // serialized by `ENV_LOCK`.
        unsafe { std::env::set_var(agent_first_ui::DELIVERY_ENV, "session") };
        assert_eq!(resolve_delivery_mode(None), Ok(UiDeliveryMode::Session));
        assert_eq!(
            resolve_delivery_mode(Some(UiDeliveryMode::Window)),
            Ok(UiDeliveryMode::Window)
        );
        // SAFETY: serialized by `ENV_LOCK`.
        unsafe { std::env::remove_var(agent_first_ui::DELIVERY_ENV) };
    }

    #[test]
    fn both_remote_deliveries_make_child_uis_join_the_session() {
        assert_eq!(child_afui_delivery(UiDeliveryMode::Window), None);
        assert_eq!(child_afui_delivery(UiDeliveryMode::Link), Some("session"));
        assert_eq!(
            child_afui_delivery(UiDeliveryMode::Session),
            Some("session")
        );
    }
}
