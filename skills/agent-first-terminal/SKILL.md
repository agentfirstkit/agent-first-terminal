---
name: agent-first-terminal
description: "Drive real terminal sessions programmatically — structured VT screen snapshots instead of scraped bytes, a multiplexed event stream, foreground signals, and input leases that let a human take the keyboard back. Trigger when running interactive or long-lived terminal programs, reading what is actually on screen, or sharing a session between an agent and a person."
allowed-tools: Bash, Read
---

# Agent-First Terminal

Use this skill when a task needs a **terminal that stays open** — an interactive
installer, a REPL, a program that redraws its screen, a session a person may
need to take over. A one-shot `Bash` call is the right tool for a command that
runs and exits; reach for afterminal when the program's state lives between
calls.

For flag-level detail, ask the command itself: `afterminal api --help` returns
every legal shape of the call at once. This skill covers behavior, decisions,
and recovery only.

## Core Rules

`ui --mode session` publishes the runtime without opening a browser here. Reach
for it whenever the person is not at this machine and already uses AFUI. The
command runs until stopped, so treat it like starting a server rather than like
opening a window. With no explicit mode the command follows `AFUI_DELIVERY` and
then defaults to `window`; never paper over an invalid environment value.
The same modes apply with `ui --api-url`: the remote API supplies a private
upstream capability, while AFUI still owns Window, Link, or Session delivery.

`ui --mode link` returns a direct trusted-LAN URL for a device that does not
have AFUI. It is a bearer capability with control of every terminal session in
the runtime. Share it only through a trusted channel, do not put it in logs or
persistent notes, and do not expose the listener directly to the Internet. It
is an AFUI-owned page governed by the global `attention.idle_timeout_s` and
`attention.grace_period_s` policy, with the same warning and renew action as
`afui session serve`.

Commands started inside either remote delivery receive
`AFUI_DELIVERY=session`, so their own UIs join the existing registry instead of
opening a local window or advertising another link.

The person who reaches a remote terminal can end it from there, exactly as if
they had closed the window in the default mode. Ending it
exits the command and kills whatever the PTY is running — including a
long-running agent you started inside it. That is almost always what was
wanted, but it happens on a single remote action, so do not treat an
unexpected exit of a session you opened this way as a crash to diagnose before
checking whether a person simply ended it.

- Treat stdout as the protocol: parse Agent-First Data events. Exit 2 means the
  invocation was rejected before anything ran and `error.code` is a `cli_*`
  code — retrying it unchanged cannot help. Exit 1 means the command ran and
  failed, with its own domain `error.code`.
- Read the **screen**, not the byte stream. A VT snapshot is what a person would
  see after the program finished redrawing; raw output still contains the cursor
  moves and repaints that produced it. Scraping the stream reintroduces exactly
  the ambiguity the snapshot removes.
- One session is one program's lifetime. Opening a second session to "retry"
  leaves the first one running and holding its PTY.

## A screen settles; it is not instant

Terminal programs redraw. A snapshot taken the moment after a keystroke shows
the state before the program reacted, and that is a real reading, not a stale
one. When a result depends on the program having caught up, wait for the event
that says so rather than sampling the screen in a loop and hoping.

## Input leases: the human wins

Multiple actors can hold input on one session, and a person can preempt the
agent. Losing the lease is not an error to retry — it means someone took the
keyboard deliberately. Stop writing input, report that control changed, and do
not reclaim the lease to finish what you were doing unless you are asked to.

- **Take a lease before writing.** Every actor except a person needs one, for
  input and for signals alike. `input_lease_required` means take one, not
  retry.
- **`human` is not an actor you can be.** It is issued by the local interface,
  where someone is actually at the keyboard; a request that declares it is
  refused with `invalid_actor`. That is what makes lease preemption and the
  secret-input window mean anything — they are the person's, not a label any
  caller can wear.

## Secret input mode: stop, do not work around it

A session can be taking a secret — a password, a recovery phrase, a key. While
it is, you cannot type into it, signal it, take a lease on it, or read its
screen, and its output produces no events. This is not a fault and not a rate
limit: a person is deliberately entering something you must not see.

- A `secret_input_active` refusal means wait, not retry-in-a-loop. The event
  stream says when the window ends; that is the signal to resume.
- Do not route around it — do not open a second session on the same program, do
  not read the raw stream, do not ask the person to turn it off so you can
  continue. You cannot turn it off yourself, by design, and the API enforces
  that rather than trusting you not to try.
- You may turn it *on*. If you are about to make a person type a credential —
  you hit a password prompt, or you are asking them to paste a key — start
  secret input first and say so. Being wrong costs a few seconds of withheld
  output; being wrong the other way puts their secret in a stream.
- What is on screen after the window closes is visible again. If a secret is
  still displayed, that is content the person can see too — say so rather than
  reading it back to them.

## Signals go to the foreground process group

Interrupting means signalling the group the terminal is actually running, not
the session host. Send the signal and then read the screen to learn what it did:
a program may trap, prompt, or ignore it. Treating "signal sent" as "program
stopped" is how a task reports success against something still running.

## Recovery

- `cli_unregistered_combination` means every flag is spelled correctly but the
  mixture is not a registered shape. Read the shapes in `--help` and pick one;
  adding flags makes a match less likely, not more.
- A session that will not open is usually resource exhaustion (PTYs, file
  descriptors), not a bad argument. Close sessions you finished with; a session
  left open outlives the call that made it.
- If output looks truncated or interleaved, you are probably reading the raw
  stream where a snapshot was wanted. Switch, rather than adding parsing.

## Setup Checklist

```bash
afterminal --version || cargo install agent-first-terminal --locked --features api
afterminal skill install       # installs this skill for codex, claude-code, opencode, hermes
```
