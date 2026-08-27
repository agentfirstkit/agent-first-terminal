#!/usr/bin/env python3
"""Exercise the compiled API over a real loopback TCP socket."""

from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import smoke_support  # noqa: E402


TOKEN = "terminal-smoke-0123456789-abcdefghijkl"
# What a person would paste into a terminal and never want on the event bus.
SECRET_VALUE = "correct-horse-battery-staple-4718"


def request(
    api_url: str,
    method: str,
    path: str,
    payload: dict[str, object] | None = None,
    *,
    authenticated: bool = True,
) -> tuple[int, object | None]:
    """Call the API and return the AFDATA envelope's payload.

    Every domain response is checked here rather than in one place that could
    rot: a success must be a `result` envelope with a trace, a failure an
    `error` envelope. Callers get `result` (or the whole error envelope).
    """
    data = None if payload is None else json.dumps(payload).encode("utf-8")
    headers = {}
    if authenticated:
        headers["Authorization"] = f"Bearer {TOKEN}"
    if data is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(
        f"{api_url}{path}",
        data=data,
        headers=headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            body = response.read()
            return response.status, unwrap(path, response.status, body)
    except urllib.error.HTTPError as error:
        body = error.read()
        return error.code, unwrap(path, error.code, body)


def ui_credential(api_url: str, attachment: object) -> str:
    """The credential out of the URL the API minted.

    The API hands back the whole URL because the authority in it is the one it
    answers UI requests under; a caller that assembled its own would be
    guessing. These tests dial that same authority, so they take the last path
    segment and keep using relative paths.
    """
    url = attachment.get("ui_access_url_secret") if isinstance(attachment, dict) else None
    if not isinstance(url, str) or not url.startswith(f"{api_url}/ui/"):
        raise RuntimeError(f"UI attachment URL is not on this API: {attachment}")
    return url.rstrip("/").rsplit("/", 1)[-1]


def unwrap(path: str, status: int, body: bytes) -> object | None:
    if not body:
        return None
    envelope = json.loads(body)
    # Discovery documents are their own contract, and the UI attachment routes
    # are private browser plumbing outside the OpenAPI surface; neither is a
    # domain response, so neither is enveloped.
    if (
        path.startswith("/openapi.json")
        or path.startswith("/schemas/")
        or path.startswith("/ui-attachments")
        or path.startswith("/ui/")
    ):
        return envelope
    if 200 <= status < 300:
        if not isinstance(envelope, dict) or envelope.get("kind") != "result":
            raise RuntimeError(f"{path} is not an AFDATA result envelope: {envelope}")
        if not isinstance(envelope.get("trace", {}).get("duration_ms"), int):
            raise RuntimeError(f"{path} has no trace: {envelope}")
        return envelope["result"]
    if not isinstance(envelope, dict) or envelope.get("kind") != "error":
        raise RuntimeError(f"{path} is not an AFDATA error envelope: {envelope}")
    if not isinstance(envelope.get("error", {}).get("retryable"), bool):
        raise RuntimeError(f"{path} error has no retryable flag: {envelope}")
    return envelope


class StreamReader:
    """Collect an SSE stream in the background until it is stopped."""

    def __init__(self, api_url: str, path: str, *, authenticated: bool) -> None:
        headers = {"Authorization": f"Bearer {TOKEN}"} if authenticated else {}
        self._request = urllib.request.Request(f"{api_url}{path}", headers=headers)
        self.lines: list[str] = []
        self._response: object | None = None
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _run(self) -> None:
        try:
            self._response = urllib.request.urlopen(self._request, timeout=30)
            for line in self._response:
                self.lines.append(line.decode("utf-8", "replace").rstrip("\n"))
        except Exception:  # noqa: BLE001 - the stop below closes this socket
            return

    def stop(self) -> list[str]:
        response = self._response
        if response is not None:
            response.close()
        self._thread.join(timeout=2)
        return list(self.lines)

    def data_payloads(self) -> list[dict[str, object]]:
        payloads = []
        for line in self.lines:
            if line.startswith("data:"):
                try:
                    payloads.append(json.loads(line[len("data:") :].strip()))
                except json.JSONDecodeError:
                    continue
        return payloads

    def raw_output(self) -> bytes:
        chunks = bytearray()
        for line in self.lines:
            if line.startswith("data:"):
                try:
                    chunks += base64.b64decode(line[len("data:") :].strip())
                except (ValueError, TypeError):
                    continue
        return bytes(chunks)


def request_text(api_url: str, path: str) -> tuple[int, str]:
    req = urllib.request.Request(f"{api_url}{path}", method="GET")
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            return response.status, response.read().decode("utf-8")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8")


def wait_for_ready(process: subprocess.Popen[str]) -> dict[str, object]:
    if process.stdout is None:
        raise RuntimeError("afterminal stdout is unavailable")
    reader = smoke_support.LineReader(process)
    deadline = time.monotonic() + 15
    lines: list[str] = []
    while time.monotonic() < deadline:
        line = reader.next_line(0.25)
        if line is None:
            if process.poll() is not None:
                raise RuntimeError(
                    f"afterminal exited before ready ({process.returncode}): {' | '.join(lines)}"
                )
            continue
        lines.append(line.rstrip())
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        progress = event.get("progress", {})
        if event.get("kind") == "progress" and progress.get("phase") == "api_ready":
            if isinstance(progress.get("api_url"), str):
                return progress
    raise RuntimeError(f"timed out waiting for API readiness: {' | '.join(lines)}")


def wait_for_marker(api_url: str, session_id: str, marker: str) -> None:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        status, screen = request(
            api_url,
            "GET",
            f"/v1/sessions/{session_id}/screen",
        )
        if status != 200 or not isinstance(screen, dict):
            raise RuntimeError(f"screen request failed: {status} {screen}")
        lines = screen.get("lines")
        if isinstance(lines, list) and marker in "\n".join(map(str, lines)):
            return
        time.sleep(0.05)
    raise RuntimeError(f"terminal {session_id} never displayed {marker}")


def wait_for_status(api_url: str, session_id: str, expected: str) -> None:
    deadline = time.monotonic() + 5
    seen = None
    while time.monotonic() < deadline:
        status, listed = request(api_url, "GET", "/v1/sessions")
        if status != 200 or not isinstance(listed, dict):
            raise RuntimeError(f"session list failed: {status} {listed}")
        for session in listed.get("sessions", []):
            if session.get("session_id") == session_id:
                seen = session.get("status")
                if seen == expected:
                    return
        time.sleep(0.05)
    raise RuntimeError(f"terminal {session_id} never reached {expected} (saw {seen})")


def secret_input_never_leaves_the_runtime(api_url: str) -> None:
    """Type a secret into a live session and prove it reached nobody.

    This process smoke checks the public machine projections: the multiplexed
    event stream and the authoritative screen. The AFUI retained-state
    projection is driven separately through AFUI's `RuntimeClient` test
    support, so this spore does not carry its own copy of the UI protocol.
    """
    session_id = "secret_probe"
    status, result = request(
        api_url,
        "POST",
        "/v1/sessions",
        {"session_id": session_id, "program": smoke_support.shell_program()},
    )
    if status != 200 or result.get("secret_input") is not False:
        raise RuntimeError(f"open secret session failed: {status} {result}")

    status, lease = request(
        api_url,
        "POST",
        f"/v1/sessions/{session_id}/leases",
        {"actor": {"kind": "agent", "id": "smoke-agent-secret"}, "mode": "shared"},
    )
    if status != 200:
        raise RuntimeError(f"acquire lease failed: {status} {lease}")
    lease_id = lease["lease_id"]

    events = StreamReader(api_url, "/v1/events", authenticated=True)
    time.sleep(0.2)
    try:
        write_input(api_url, session_id, "printf 'BEFORE_SECRET\\n'\n")
        wait_for_marker(api_url, session_id, "BEFORE_SECRET")
        status, started = request(
            api_url,
            "POST",
            f"/v1/sessions/{session_id}/secret-input/actions",
            {
                "action": "start",
                "actor": {"kind": "controller", "id": "smoke-controller"},
                "reason": "password prompt",
            },
        )
        if status != 200 or started.get("secret_input") is not True:
            raise RuntimeError(f"start secret input failed: {status} {started}")

        # An agent holding a live lease is refused input while it is on.
        status, refused = request(
            api_url,
            "POST",
            f"/v1/sessions/{session_id}/input",
            {
                "actor": {"kind": "agent", "id": "smoke-agent-secret"},
                "lease_id": lease_id,
                "data_base64": base64.b64encode(b"whoami\n").decode("ascii"),
            },
        )
        if status != 409 or refused["error"]["code"] != "secret_input_active":
            raise RuntimeError(f"an agent was not suspended: {status} {refused}")
        if refused["error"]["retryable"] is not True:
            raise RuntimeError(f"suspension must be retryable: {refused}")
        status, refused_exit = request(
            api_url,
            "POST",
            f"/v1/sessions/{session_id}/secret-input/actions",
            {"action": "end", "actor": {"kind": "agent", "id": "smoke-agent-secret"}},
        )
        if status != 403 or refused_exit["error"]["code"] != "secret_input_exit_denied":
            raise RuntimeError(f"an agent could end secret input: {status} {refused_exit}")

        # Nobody reaching this API can type into the window either, so the
        # screen stays withheld for every caller here.
        status, screen = request(api_url, "GET", f"/v1/sessions/{session_id}/screen")
        if status != 200 or screen.get("secret_input") is not True or screen["lines"] != []:
            raise RuntimeError(f"the screen was not withheld: {status} {screen}")

        # What a person does with the window — typing into it and closing it —
        # goes through the local interface, which is in-process and has no HTTP
        # route. `secret_input_suspends_agents_and_withholds_the_screen` covers
        # that side, including that the typed secret never reappears once
        # publication resumes.
        end_secret_input(api_url, session_id)
        time.sleep(0.3)
    finally:
        event_lines = events.stop()
        event_payloads = events.data_payloads()
    status, screen = request(api_url, "GET", f"/v1/sessions/{session_id}/screen")
    if status != 200 or not isinstance(screen, dict):
        raise RuntimeError(f"read post-secret screen failed: {status} {screen}")
    lines = screen.get("lines")
    published = "\n".join(line for line in lines or [] if isinstance(line, str))
    if SECRET_VALUE in published:
        raise RuntimeError(f"the secret reached the authoritative screen: {published!r}")
    if SECRET_VALUE in "\n".join(event_lines):
        raise RuntimeError("the secret reached the event stream")

    kinds = [payload.get("event", {}).get("type") for payload in event_payloads]
    if "secret_input_started" not in kinds:
        raise RuntimeError(f"the window was not announced on the event stream: {kinds}")
    opened = kinds.index("secret_input_started")
    inside = kinds[opened + 1 :]
    leaked = [kind for kind in inside if kind != "input_rejected"]
    if leaked:
        raise RuntimeError(f"the secret window published {leaked}")
    if "input_rejected" not in inside:
        raise RuntimeError("the suspended agent's refusal was not announced")

    status, result = request(api_url, "DELETE", f"/v1/sessions/{session_id}")
    if status != 204:
        raise RuntimeError(f"close secret session failed: {status} {result}")


_LEASES: dict[str, str] = {}


def controller_lease(api_url: str, session_id: str) -> str:
    """A shared lease for this smoke client.

    Nothing reaching the HTTP API is the person at the keyboard, so nothing
    reaching it may say `kind:"human"` — which used to be how this test wrote
    input without a lease at all. A lease is the ordinary shape of an API
    caller.
    """
    cached = _LEASES.get(session_id)
    if cached is not None:
        return cached
    status, result = request(
        api_url,
        "POST",
        f"/v1/sessions/{session_id}/leases",
        {
            "actor": {"kind": "controller", "id": "smoke-controller"},
            "mode": "shared",
            "ttl_ms": 60_000,
        },
    )
    if status != 200 or not isinstance(result, dict) or "lease_id" not in result:
        raise RuntimeError(f"lease for {session_id} failed: {status} {result}")
    lease_id = str(result["lease_id"])
    _LEASES[session_id] = lease_id
    return lease_id


def write_input(api_url: str, session_id: str, command: str) -> None:
    status, result = request(
        api_url,
        "POST",
        f"/v1/sessions/{session_id}/input",
        {
            "actor": {"kind": "controller", "id": "smoke-controller"},
            "lease_id": controller_lease(api_url, session_id),
            "data_base64": base64.b64encode(command.encode("utf-8")).decode("ascii"),
        },
    )
    if status != 200 or result.get("accepted") is not True:
        raise RuntimeError(f"controller input failed: {status} {result}")


def end_secret_input(api_url: str, session_id: str) -> None:
    """Assert that nothing over this API can end the window.

    The skill has always told agents "you cannot turn it off yourself, by
    design". It is true now: `human` is issued by the local interface, and a
    request body claiming it is refused outright. The person closes the window;
    an API client that has nothing left to do closes the session.
    """
    status, refused = request(
        api_url,
        "POST",
        f"/v1/sessions/{session_id}/secret-input/actions",
        {"action": "end", "actor": {"kind": "controller", "id": "smoke-controller"}},
    )
    if status == 200 or refused["error"]["code"] != "secret_input_exit_denied":
        raise RuntimeError(f"an agent ended a secret window: {status} {refused}")

    status, claimed = request(
        api_url,
        "POST",
        f"/v1/sessions/{session_id}/secret-input/actions",
        {"action": "end", "actor": {"kind": "human", "id": "smoke-human"}},
    )
    if status != 400 or claimed["error"]["code"] != "invalid_actor":
        raise RuntimeError(f"a bearer closed a window by calling itself human: {status} {claimed}")


def main() -> int:
    if len(sys.argv) != 2:
        raise RuntimeError("usage: api_smoke.py PATH_TO_AFTERMINAL")
    environment = os.environ.copy()
    environment["AFTERMINAL_API_ACCESS_TOKEN_SECRET"] = TOKEN
    process = subprocess.Popen(
        [sys.argv[1], "api", "serve", "--port", "0", "--mode", "local"],
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        ready = wait_for_ready(process)
        api_url = ready["api_url"]
        if ready.get("mode") != "local" or not api_url.startswith("http://127.0.0.1:"):
            raise RuntimeError(f"--mode local did not bind loopback: {ready}")
        if ready.get("schema_index_url") != f"{api_url}/schemas/index.json":
            raise RuntimeError(f"ready event has no usable schema index url: {ready}")
        if TOKEN in json.dumps(ready):
            raise RuntimeError("the ready event echoed the bearer credential")
        status, contract = request(
            api_url,
            "GET",
            "/openapi.json",
            authenticated=False,
        )
        if status != 200 or not isinstance(contract, dict) or contract.get("openapi") != "3.2.0":
            raise RuntimeError(f"OpenAPI discovery failed: {status} {contract}")
        paths = contract.get("paths")
        if isinstance(paths, dict) and "/ui-attachments" in paths:
            raise RuntimeError("private UI attachment route leaked into OpenAPI")

        # The standalone Schemas are part of the fixed discovery face, and they
        # have to be readable without a credential.
        status, index = request(api_url, "GET", "/schemas/index.json", authenticated=False)
        entries = index.get("schemas") if isinstance(index, dict) else None
        if status != 200 or not isinstance(entries, list) or not entries:
            raise RuntimeError(f"schema index discovery failed: {status} {index}")
        if index.get("count") != len(entries):
            raise RuntimeError(f"schema index count disagrees with its own list: {index}")
        for entry in entries:
            status, schema = request(api_url, "GET", entry["schema_url"], authenticated=False)
            if status != 200 or not isinstance(schema, dict) or "$id" not in schema:
                raise RuntimeError(f"schema {entry['schema_url']} failed: {status} {schema}")
            if "$ref" in json.dumps(schema):
                raise RuntimeError(f"schema {entry['schema_url']} is not self-contained")

        for session_id in ("external_a", "external_b"):
            status, result = request(
                api_url,
                "POST",
                "/v1/sessions",
                {
                    "session_id": session_id,
                    "program": smoke_support.shell_program(),
                    "rows": 24,
                    "cols": 80,
                },
            )
            if status != 200:
                raise RuntimeError(f"open {session_id} failed: {status} {result}")

        status, attachment = request(
            api_url,
            "POST",
            "/ui-attachments",
            {"initial_session_id": "external_b"},
        )
        if status != 200 or attachment.get("initial_session_id") != "external_b":
            raise RuntimeError(f"issue UI attachment failed: {status} {attachment}")
        ui_token = ui_credential(api_url, attachment)
        status, page = request_text(api_url, f"/ui/{ui_token}/")
        if status != 200 or "Shared terminal sessions" not in page:
            raise RuntimeError(f"attached UI page failed: {status}")
        status, result = request(
            api_url,
            "DELETE",
            f"/ui-attachments/{ui_token}",
        )
        if status != 204 or result is not None:
            raise RuntimeError(f"revoke UI attachment failed: {status} {result}")
        status, _page = request_text(api_url, f"/ui/{ui_token}/")
        if status != 404:
            raise RuntimeError(f"revoked UI capability still works: {status}")

        for session_id, marker in (
            ("external_a", "EXTERNAL_A"),
            ("external_b", "EXTERNAL_B"),
        ):
            command = f"printf '{marker}\\n'\n".encode("utf-8")
            status, result = request(
                api_url,
                "POST",
                f"/v1/sessions/{session_id}/input",
                {
                    "actor": {"kind": "controller", "id": "smoke-controller"},
                    "lease_id": controller_lease(api_url, session_id),
                    "data_base64": base64.b64encode(command).decode("ascii"),
                },
            )
            if status != 200 or not isinstance(result, dict) or result.get("accepted") is not True:
                raise RuntimeError(f"input {session_id} failed: {status} {result}")
            wait_for_marker(api_url, session_id, marker)

        shared_leases: dict[str, str] = {}
        for actor_id in ("smoke-agent-a", "smoke-agent-b"):
            status, result = request(
                api_url,
                "POST",
                "/v1/sessions/external_a/leases",
                {
                    "actor": {"kind": "agent", "id": actor_id},
                    "mode": "shared",
                    "ttl_ms": 60_000,
                },
            )
            lease_id = result.get("lease_id") if isinstance(result, dict) else None
            if status != 200 or not isinstance(lease_id, str):
                raise RuntimeError(f"acquire shared lease failed: {status} {result}")
            shared_leases[actor_id] = lease_id

        for actor_id, marker in (
            ("smoke-agent-a", "SHARED_AGENT_A"),
            ("smoke-agent-b", "SHARED_AGENT_B"),
        ):
            command = f"printf '{marker}\\n'\n".encode("utf-8")
            status, result = request(
                api_url,
                "POST",
                "/v1/sessions/external_a/input",
                {
                    "actor": {"kind": "agent", "id": actor_id},
                    "lease_id": shared_leases[actor_id],
                    "data_base64": base64.b64encode(command).decode("ascii"),
                },
            )
            if status != 200:
                raise RuntimeError(f"shared agent input failed: {status} {result}")
            wait_for_marker(api_url, "external_a", marker)

        status, result = request(api_url, "GET", "/v1/sessions")
        sessions = result.get("sessions") if isinstance(result, dict) else None
        session_ids = (
            [session.get("session_id") for session in sessions]
            if isinstance(sessions, list)
            else []
        )
        if status != 200 or session_ids != ["external_a", "external_b"]:
            raise RuntimeError(f"multi-session list failed: {status} {result}")

        for session_id in ("external_a", "external_b"):
            status, result = request(
                api_url,
                "DELETE",
                f"/v1/sessions/{session_id}",
            )
            if status != 204 or result is not None:
                raise RuntimeError(f"close {session_id} failed: {status} {result}")

        signal_session_id = "external_signal"
        if smoke_support.WINDOWS:
            # Windows has no signal to send a process group: `deliver_signal`
            # there supports `kill` and refuses the rest. That refusal is the
            # platform's contract rather than a gap in the smoke, so it is what
            # gets asserted — a caller has to be able to tell "this terminal
            # will not interrupt" from "this terminal did not answer".
            status, result = request(
                api_url,
                "POST",
                "/v1/sessions",
                {
                    "session_id": signal_session_id,
                    "program": smoke_support.shell_program(),
                    "args": smoke_support.shell_args(),
                },
            )
            if status != 200:
                raise RuntimeError(f"open signal session failed: {status} {result}")

            lease_id = controller_lease(api_url, signal_session_id)
            status, result = request(
                api_url,
                "POST",
                f"/v1/sessions/{signal_session_id}/signal",
                {
                    "actor": {"kind": "controller", "id": "smoke-controller"},
                    "lease_id": lease_id,
                    "signal": "interrupt",
                },
            )
            if status != 501 or not isinstance(result, dict):
                raise RuntimeError(f"interrupt was not refused: {status} {result}")
            if result.get("error", {}).get("code") != "signal_not_supported":
                raise RuntimeError(f"interrupt refused with the wrong error: {result}")

            # `kill` is the one this platform does deliver.
            status, result = request(
                api_url,
                "POST",
                f"/v1/sessions/{signal_session_id}/signal",
                {
                    "actor": {"kind": "controller", "id": "smoke-controller"},
                    "lease_id": lease_id,
                    "signal": "kill",
                },
            )
            if (
                status != 200
                or not isinstance(result, dict)
                or result.get("delivered") is not True
                or result.get("signal") != "kill"
            ):
                raise RuntimeError(f"kill signal failed: {status} {result}")
            wait_for_status(api_url, signal_session_id, "exited")
        else:
            status, result = request(
                api_url,
                "POST",
                "/v1/sessions",
                {
                    "session_id": signal_session_id,
                    "program": smoke_support.shell_program(),
                    "args": [
                        "-c",
                        "trap 'printf \"EXTERNAL_INTERRUPTED\\n\"; exit 0' INT; "
                        "printf 'EXTERNAL_SIGNAL_READY\\n'; "
                        "while :; do sleep 1; done",
                    ],
                },
            )
            if status != 200:
                raise RuntimeError(f"open signal session failed: {status} {result}")
            wait_for_marker(api_url, signal_session_id, "EXTERNAL_SIGNAL_READY")

            status, result = request(
                api_url,
                "POST",
                f"/v1/sessions/{signal_session_id}/signal",
                {
                    "actor": {"kind": "controller", "id": "smoke-controller"},
                    "lease_id": controller_lease(api_url, signal_session_id),
                    "signal": "interrupt",
                },
            )
            if (
                status != 200
                or not isinstance(result, dict)
                or result.get("delivered") is not True
                or result.get("signal") != "interrupt"
            ):
                raise RuntimeError(f"interrupt signal failed: {status} {result}")
            wait_for_marker(api_url, signal_session_id, "EXTERNAL_INTERRUPTED")

        status, result = request(
            api_url,
            "DELETE",
            f"/v1/sessions/{signal_session_id}",
        )
        if status != 204 or result is not None:
            raise RuntimeError(f"close signal session failed: {status} {result}")

        secret_input_never_leaves_the_runtime(api_url)
        return 0
    finally:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


if __name__ == "__main__":
    raise SystemExit(main())
