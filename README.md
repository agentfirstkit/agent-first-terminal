# Agent-First Terminal

Let your AI agent work in a real terminal you can watch — and take the keyboard
back whenever you want, from your phone if you are not at the machine.

`agent-first-terminal` is the multi-session PTY runtime underneath: raw terminal
fan-out, structured VT screen snapshots, a multiplexed event stream, input
writes, resize, foreground process-group signals, lifecycle control,
multi-actor input leases with human preemption, and an optional loopback HTTP
server with a generated OpenAPI 3.2 contract. Its CLI can deliver a trusted
DOM-rendered UI over that same live runtime as a local window, an AFUI-owned
LAN link, or a registered AFUI session.

> **Ask your agent:** "Open a terminal I can watch from my phone while you work."

## The problem: capturing output is not the same as having a terminal

An agent can already run a command and read what it printed. What it cannot do
is hand you the terminal — and it needs to, more often than the "run a command"
framing admits:

- a program asks for a password, and the bytes must not end up in a transcript;
- something wants a confirmation nobody anticipated, and the agent is stuck;
- a build runs for fifteen minutes and you want to glance at it from the sofa;
- a full-screen program repaints, and "capture stdout" turns it into noise;
- you want to take the keyboard for thirty seconds and hand it back.

Each of those has an improvised answer — a captured pipe, a screenshot pasted
into chat, `ssh` from your phone, a shell the agent runs blind. This is the
runtime that makes them one thing instead of five.

## Two actors, one terminal, and the person wins

The hard part is not opening a PTY. It is that an agent and a person are both
typing into one, and neither should have to guess whose turn it is.

Input goes through **leases**. An automated actor takes one — `shared` when
several agents each submit whole chunks, `exclusive` to reserve a multi-step
interaction — and the runtime serializes requests so bytes from two of them
never interleave.

A human needs no lease, and typing immediately revokes an automated exclusive
one. Not at the next boundary, not by asking: the moment a person types, the
agent holding the lease has lost it. That ordering is the design, because the
alternative is a person losing a race to software for their own keyboard.

## Secret input publishes nothing

A password prompt inside a terminal that is being streamed, snapshotted and
multiplexed is the sharpest version of the problem: not echoing it is not
enough, because the bytes are still in a screen snapshot and an event stream
that other actors read.

Secret input mode is a shield over the session. While it is on, nothing derived
from those bytes is published — no raw output to subscribers, nothing retained
for replay, no screen content, and no event carrying even the size of what was
typed. Every non-human actor is refused input, signals and leases until it ends.

Any actor may raise the shield, because that is the safe direction. Only a
person may lower it, and lowering waits for the session to fall quiet, so the
echo of what was just typed is not released as publishing resumes. Entry and
exit are events, and the window says so plainly: a protection you cannot tell is
on is worse than none.

## Three ways to reach the person

`afterminal ui` opens the live runtime as a trusted, DOM-rendered terminal page.
Where that page appears is a delivery, and the three exist because a window is
not always possible.

```sh
export AFTERMINAL_API_ACCESS_TOKEN_SECRET='replace-with-at-least-32-bearer-safe-characters'

# A window on this machine. AFUI owns the browser process, its disposable
# profile, the credential and the security headers.
afterminal ui main

# A LAN URL for a device that has no AFUI on it. Bearer capability: whoever
# holds it controls every session in this runtime.
afterminal ui main --mode link

# Publish and open nothing here — for a machine with no display, or a person
# who is somewhere else entirely.
afterminal ui main --mode session
```

`--mode session` is the shape worth understanding. A session bounded by a window
nobody is watching ends the moment someone tidies it away; published this way,
the command itself is the bound, and [`afui session
serve`](https://github.com/agentfirstkit/agent-first-ui) is how it reaches a
phone. With no `--mode`, delivery follows `AFUI_DELIVERY` and otherwise opens a
window; an invalid value in the environment is an error rather than a silent
local window.

One consequence is worth knowing before it surprises you. Every PTY the runtime
opens inherits this process's environment, including `AFUI_DELIVERY=session`
when `afterminal` itself was reached through `link` or `session`. A command with
a UI of its own, run *inside* that shell, then publishes into the existing
registry instead of opening a window on a machine nobody is looking at.
`--mode window` leaves the variable untouched, so a value you exported yourself
is never overwritten.

Ending a session from the far end closes it exactly as closing the window does:
the command exits, and with it whatever the PTY was running. If that program was
a long-running agent, ending the session is what stops it. Usually that is
precisely what the person meant — but it is one remote tap away, and it does not
ask twice.

## Attaching to a runtime that is already running

A separate `afterminal api serve` can own the sessions, and a UI command can be
pointed at it:

```sh
afterminal ui --api-url http://127.0.0.1:9418 --session-id codex --mode window
```

Attach mode creates nothing — not the runtime, not the named session. The id
selects an existing session; omit it to open the multi-session list. Several UI
commands can attach at once, and the browser never receives the API credential.

## For callers that are programs

The same runtime is available over a bearer-protected HTTP API with a committed,
drift-tested OpenAPI 3.2 contract: open and list sessions, read structured
screens, write input, resize, close, deliver `interrupt`/`terminate`/`kill`, and
watch every session's events on one SSE stream that resumes from
`Last-Event-ID`. On Unix those are real signals, not control-character writes.

Domain responses are AFDATA envelopes and pass through AFDATA redaction on the
way out. `/health`, `/openapi.json` and the schemas under `/schemas/` are public
for discovery; `/v1` requires a bearer credential and never accepts one in a
query string.

The core library has no async runtime and no HTTP dependency, and knows nothing
about UI hosts or task-completion semantics — what terminal output *means* is a
question for whoever is driving.

## Install the CLI

```bash
# prebuilt binary
brew install agentfirstkit/tap/afterminal   # macOS / Linux
scoop bucket add agentfirstkit https://github.com/agentfirstkit/scoop-bucket && scoop install afterminal   # Windows

# or from crates.io
cargo install agent-first-terminal --locked --features api
```

Prebuilt archives are also available from
[GitHub Releases](https://github.com/agentfirstkit/agent-first-terminal/releases).

`cargo install agent-first-terminal` without `--features api` installs nothing,
and says so while naming the flag, rather than leaving a working-looking install
with no executable. Depending on this crate as a library needs no feature at
all: the core has no async runtime and no HTTP dependency.

## CLI

`afterminal` emits one AFDATA protocol event per run. JSON is the default; YAML
and plain output are also available.

```bash
afterminal --version
# {"kind":"result","result":{"code":"version","display_name":"Agent-First Terminal","name":"afterminal","version":"0.1.0"},"trace":{}}

export AFTERMINAL_API_ACCESS_TOKEN_SECRET='replace-with-at-least-32-bearer-safe-characters'

# Run a coding agent in the session instead of the default shell.
afterminal ui codex --program codex

# Serve the runtime for callers that are programs.
afterminal api serve --port 9418

# Write the OpenAPI document and its standalone schemas.
afterminal api export --directory openapi --force
```

## Agent Skill

Then install the embedded [Agent Skill](skills/agent-first-terminal/SKILL.md) so
the agent follows afterminal's behavior rules — when to take a lease and which
kind, that a person's keystroke outranks it, and that secret mode is the
person's to end. `skill install` targets Codex, Claude Code, opencode and
Hermes; `skill status` reports whether each install is present, valid and
current:

```bash
afterminal skill install --agent all --scope workspace
afterminal skill status
```

## Replacing the terminal page

The page is a MiniJinja template and it is yours to restructure — reorder the
rail, drop the footer, rename every heading:

```sh
afui frontend init --provider-id afterminal --ui-kind terminal
afui frontend enable
```

Two things stay afterminal's: the elements the runtime binds to (take their ids
from `document.elements`, the process controls from `document.signals` and the
key bar from `document.keys`), and the script, which a frontend cannot supply at
all. A page missing a required element is reported as a broken override rather
than opening as a terminal that quietly does nothing.

See [docs/reference.md](docs/reference.md) for the lease, signal, API and
override details.

## Docs

- [Reference](docs/reference.md) — leases, secret mode, API surface, override contract
- [CLI](docs/cli.md) — generated command and flag reference

## License

MIT
