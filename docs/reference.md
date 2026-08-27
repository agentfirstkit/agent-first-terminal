# Reference

Detail that the README deliberately leaves out. For the command and flag
listing, see [cli.md](cli.md); for the behavioural rules an agent should follow,
see the agent skill.

## Input leases

Every HTTP input and signal identifies its actor. Non-human actors acquire a
lease before acting.

| Lease | Who it is for | What it guarantees |
| --- | --- | --- |
| `shared` | several agents each submitting complete chunks | the runtime serializes each request, so bytes from two requests never interleave |
| `exclusive` | one automated actor across a multi-step interaction | no other automated actor may write until it is released, expires, or a human preempts it |
| none | a person | input is always accepted, and immediately revokes a non-human exclusive lease |

A human may also take an exclusive lease for a longer manual takeover. Leases
use monotonic TTL deadlines, can be renewed, and disappear on release, expiry or
preemption. Browser input from the stock page is identified as
`human:local-ui`, so opening the window and typing preempts an agent without any
further ceremony.

## Secret input mode

While a session is in secret input mode it publishes nothing derived from the
typed bytes:

- no raw output to subscribers;
- nothing retained for replay;
- no screen content;
- no event carrying the size of what was typed or echoed.

Every non-human actor is refused input, signals and leases for the duration.

Any actor may enter secret mode — raising the shield is the safe direction. Only
a human actor may leave it, and leaving waits for the session to fall quiet so
the echo of what was just typed is not released as publication resumes. Entry
and exit are both events, and the stock page marks the state unmistakably.

## Composing with an input method

The field an IME composes into sits on the cursor cell, and is shown while a
composition is in progress: the terminal's own font, underlined, sized to the
text being composed with wide characters counted as two columns. It returns to
being invisible when the composition commits.

Two things follow from that, and both are the reason it is done this way. The
half-typed word appears where it is being typed rather than nowhere, because the
browser keeps the composing text in that field's value. And a candidate window
follows the cursor, because an input method anchors it to the focused field's
caret — with the field parked in a corner, so was the candidate list.

A commit sends the composed text as one write. The first keystroke *after* a
commit is ordinary input and is sent as itself; on a Chinese keyboard that is
usually the space between two words.

## HTTP API

`api serve --mode local` (the default) binds `127.0.0.1`. `--mode lan` binds all
interfaces for a trusted IPv4 network and publishes this machine's LAN address.
There is no TLS and no Internet exposure built in.

Public for discovery: `/health`, `/openapi.json`, and the standalone JSON
Schemas under `/schemas/`. Everything under `/v1` requires
`Authorization: Bearer ...` and never accepts a credential in a query string.

Callers can:

- open one or many sessions, and list them;
- read structured VT screens;
- write base64-encoded input bytes;
- resize or close a session;
- deliver `interrupt`, `terminate` or `kill` to the foreground process group —
  real `SIGINT`/`SIGTERM`/`SIGKILL` on Unix, not control-character writes;
- watch every session's events over one SSE stream that resumes from
  `Last-Event-ID`.

Domain responses are AFDATA envelopes and pass through AFDATA redaction on the
way out.

The OpenAPI document is committed and drift-tested against the code:

```sh
afterminal api export --directory openapi --force
```

## Delivery

Delivery is hosted by [Agent-First
UI](https://github.com/agentfirstkit/agent-first-ui), which owns the browser
process and disposable profile for `window`, the outer page and attention policy
for `link`, and the registry entry for `session`. The upstream UI credential
stays private inside the CLI. `--port` always names the bearer-protected API,
never the delivery.

For `link`, AFUI applies its global `attention.idle_timeout_s` and
`attention.grace_period_s` policy, including the warning and the renew action.
The terminal page's once-per-second refresh only keeps its own session list
current; it does not choose the URL's lifetime.

In attach mode (`ui --api-url`), the running API issues a separate private UI
capability. The CLI keeps that capability alive behind the delivery and revokes
it when the delivery ends; the server-side idle timeout on it is crash cleanup,
not the person-facing lifetime. `ui --api-url` resolves no frontend locally —
the machine serving that runtime decides which page it serves.

## Override contract

`ui_api_version` is `3`. A frontend may supply `templates/page.html.j2`,
`style.css` and `assets/**`, each independently; a file it does not supply is
afterminal's.

Two things are not the frontend's:

- **The elements the runtime binds to.** Take their ids from
  `document.elements` rather than typing them out, the process controls from
  `document.signals`, and the key bar from `document.keys`. `data-signal` and
  `data-key` are what `app.js` reads, so a control declares which signal or key
  it is and afterminal decides what sending it does. A page that drops a
  required element is reported as a broken override; it never opens as a
  terminal that quietly does nothing.
- **The script.** `<!-- afterminal:trusted-runtime -->` is where afterminal
  splices in `app.js`. A frontend cannot supply JavaScript at all — AFUI refuses
  a file whose name says it is a script, and refuses one hidden inside a
  template — so a pinned artifact stays pinned.

A frontend afterminal cannot load ends the command with `ui_frontend_unusable`
and opens no window; it is never a quietly substituted built-in page.
`AFUI_SAFE_MODE=1` ignores every override.

## Scope

The runtime has no `agent-first-ui` dependency and does not know application or
task-completion semantics. Higher-level controllers decide what terminal output
means.
