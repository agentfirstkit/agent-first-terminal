#!/usr/bin/env python3
"""Drive `afterminal ui` with a stub browser and read what the window would.

The window is a real child process to afterminal: AFUI launches whatever
`AFUI_BROWSER_BINARY` names and treats its exit as the person closing the
window. The stub here plays that part, fetches the page and every asset the
page references, then exits — which must end the session.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import smoke_support  # noqa: E402


TOKEN = "terminal-ui-smoke-0123456789-abcdefg"
SESSION_ID = "uismoke"

# Exactly what page.html references, relative to a URL that ends in `/`.
PAGE_ASSETS = (
    "app.js",
    "style.css",
)

STUB_BROWSER = '''#!/usr/bin/env python3
"""Stand in for the Chromium window AFUI would open."""

import json
import os
import sys
import urllib.error
import urllib.request
from urllib.parse import urljoin

ASSETS = {assets!r}


def fetch(url):
    try:
        with urllib.request.urlopen(url, timeout=10) as response:
            return response.status, response.read().decode("utf-8", "replace"), response.headers
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8", "replace"), error.headers
    except OSError as error:
        return 0, str(error), {{}}


def main():
    app = ""
    for argument in sys.argv[1:]:
        if argument.startswith("--app="):
            app = argument[len("--app=") :]
    report = {{"app_url": app, "fetched": {{}}}}
    if app:
        status, body, headers = fetch(app)
        report["fetched"]["/"] = {{"status": status, "bytes": len(body)}}
        report["page_has_title"] = "Shared terminal sessions" in body
        report["page_body"] = body
        report["page_csp"] = headers.get("content-security-policy", "")
        # `urljoin` is how a browser resolves the page's relative references.
        for asset in ASSETS:
            status, body, _ = fetch(urljoin(app, asset))
            report["fetched"][asset] = {{"status": status, "bytes": len(body)}}
            if asset == "app.js":
                report["app_js_body"] = body
        status, body, _ = fetch(urljoin(app, "sessions"))
        report["fetched"]["sessions"] = {{"status": status, "bytes": len(body)}}
        report["sessions_body"] = body
    with open(os.environ["AFTERMINAL_UI_SMOKE_REPORT"], "w", encoding="utf-8") as handle:
        json.dump(report, handle)
    # Exiting is the person closing the window.
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
'''


def request(api_url: str, path: str, *, authenticated: bool) -> tuple[int, object | None]:
    headers = {"Authorization": f"Bearer {TOKEN}"} if authenticated else {}
    req = urllib.request.Request(f"{api_url}{path}", headers=headers, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            body = response.read()
            return response.status, json.loads(body) if body else None
    except urllib.error.HTTPError as error:
        return error.code, None
    except OSError:
        return 0, None


def read_events(
    reader: smoke_support.LineReader,
    process: subprocess.Popen[str],
    phase: str,
    timeout: float,
) -> tuple[dict, list[str]]:
    """Consume stdout until an event with `phase` arrives."""
    deadline = time.monotonic() + timeout
    lines: list[str] = []
    while time.monotonic() < deadline:
        line = reader.next_line(0.25)
        if line is None:
            if process.poll() is not None:
                raise RuntimeError(
                    f"afterminal exited before {phase} ({process.returncode}): {' | '.join(lines)}"
                )
            continue
        lines.append(line.rstrip())
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        progress = event.get("progress", {})
        if event.get("kind") == "progress" and progress.get("phase") == phase:
            return progress, lines
    raise RuntimeError(f"timed out waiting for {phase}: {' | '.join(lines)}")


def main() -> int:
    if len(sys.argv) != 2:
        raise RuntimeError("usage: ui_smoke.py PATH_TO_AFTERMINAL")
    binary = sys.argv[1]
    with tempfile.TemporaryDirectory(prefix="afterminal-ui-smoke-") as workspace:
        report_path = os.path.join(workspace, "report.json")
        stub_path = smoke_support.write_python_launcher(
            Path(workspace), "stub-browser", STUB_BROWSER.format(assets=PAGE_ASSETS)
        )

        environment = os.environ.copy()
        environment["AFTERMINAL_API_ACCESS_TOKEN_SECRET"] = TOKEN
        environment["AFUI_BROWSER_BINARY"] = stub_path
        environment["AFTERMINAL_UI_SMOKE_REPORT"] = report_path
        # An explicit window must win over a remote-delivery environment.
        environment["AFUI_DELIVERY"] = "session"
        process = subprocess.Popen(
            [
                binary,
                "ui",
                SESSION_ID,
                "--port",
                "0",
                "--program",
                smoke_support.shell_program(),
                "--mode",
                "window",
            ],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        reader = smoke_support.LineReader(process)
        try:
            ready, lines = read_events(reader, process, "ui_ready", 20)
            api_url = ready.get("api_url")
            # `mode` is the shape of the `ui` call — a window here, or a session
            # published for somebody who is not. `api_mode` is the listener
            # `--port` names, which for either is loopback.
            if (
                not isinstance(api_url, str)
                or ready.get("mode") != "window"
                or ready.get("api_mode") != "local"
            ):
                raise RuntimeError(f"unexpected ui_ready event: {ready}")
            if ready.get("initial_session_id") != SESSION_ID:
                raise RuntimeError(f"ui_ready lost the initial session: {ready}")

            # `--port` still names the API, and it is still bearer-protected.
            status, sessions = request(api_url, "/v1/sessions", authenticated=True)
            result = sessions.get("result", {}) if isinstance(sessions, dict) else {}
            ids = [entry.get("session_id") for entry in result.get("sessions", [])]
            if status != 200 or ids != [SESSION_ID]:
                raise RuntimeError(f"API listener did not serve the session: {status} {sessions}")
            if sessions.get("kind") != "result":
                raise RuntimeError(f"the API listener dropped the envelope: {sessions}")
            status, _ = request(api_url, "/v1/sessions", authenticated=False)
            if status != 401:
                raise RuntimeError(f"API listener accepted an unauthenticated read: {status}")
            # The window is served by AFUI on its own listener, so the API port
            # holds no UI capability at all.
            status, _ = request(api_url, "/ui/any-token/", authenticated=False)
            if status != 404:
                raise RuntimeError(f"API listener served a UI page: {status}")

            # The stub exits as soon as it has fetched everything, which is the
            # person closing the window, so afterminal must stop on its own.
            remaining = process.wait(timeout=20)
            if remaining != 0:
                raise RuntimeError(f"afterminal exited {remaining} after the window closed")
            lines.extend(reader.drain())
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)

        with open(report_path, encoding="utf-8") as handle:
            report = json.load(handle)

        app_url = report.get("app_url", "")
        if not app_url.endswith("/"):
            raise RuntimeError(f"the window URL must end in a directory: {app_url!r}")
        if not report.get("page_has_title"):
            raise RuntimeError("the window did not receive the terminal page")
        # Styled server cells use inline color properties.
        if "style-src 'self' 'unsafe-inline'" not in report.get("page_csp", ""):
            raise RuntimeError(f"the page lost its stylesheet policy: {report.get('page_csp')!r}")
        for path in PAGE_ASSETS:
            outcome = report.get("fetched", {}).get(path, {})
            if outcome.get("status") != 200 or outcome.get("bytes", 0) <= 0:
                raise RuntimeError(f"the window could not load {path}: {outcome}")
        private_api = report.get("fetched", {}).get("sessions", {})
        if private_api.get("status") != 404:
            raise RuntimeError(f"the window still exposes a private UI API: {private_api}")
        if TOKEN in report.get("page_body", "") or TOKEN in report.get("sessions_body", ""):
            raise RuntimeError("the API bearer reached browser-readable content")
        app_js = report.get("app_js_body", "")
        if "afui.connect({" not in app_js:
            raise RuntimeError("the terminal page did not connect through AFUI")
        for forbidden in ("EventSource", "afui.stream(", "afui.request(", "/sessions"):
            if forbidden in app_js:
                raise RuntimeError(f"the terminal page still owns UI transport: {forbidden}")

        # The URL carries the UI capability, so nothing may print it.
        credential = app_url.rstrip("/").rsplit("/", 1)[-1]
        output = "\n".join(lines)
        if credential and credential in output:
            raise RuntimeError("the UI capability was emitted in ordinary output")
        if not any(
            json.loads(line).get("kind") == "result"
            for line in lines
            if line.startswith("{") and _is_json(line)
        ):
            raise RuntimeError(f"afterminal emitted no terminal result: {output}")
    return 0


def _is_json(line: str) -> bool:
    try:
        json.loads(line)
    except json.JSONDecodeError:
        return False
    return True


if __name__ == "__main__":
    raise SystemExit(main())
