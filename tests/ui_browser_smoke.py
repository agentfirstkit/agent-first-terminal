#!/usr/bin/env python3
"""Type into the terminal page from a real browser, and read the PTY's answer.

`ui_smoke.py` proves the window is served: it plays the browser AFUI launches,
fetches the page and every asset, and exits so the session ends. What it cannot
say is whether a keystroke in that page reaches the process on the other side —
its stub never runs the script it downloads.

This drives Chromium instead. AFUI still launches a stub, because the stub's
lifetime *is* the window's and killing it is how the session ends; Chromium is
opened separately at the URL the stub was handed. Keys go in through
`Input.dispatchKeyEvent`, which is a real key event down the page's own
listeners, and the assertion is made against the authoritative VT screen read
back over the HTTP API — not against the DOM, which would only prove the page
agrees with itself.

What this does not cover, and cannot: Chromium is not Safari, and a synthesized
composition is not an IME. Mobile input, rotation and the on-screen key bar stay
on the device matrix a release records by hand.

CDP travels over `--remote-debugging-pipe` rather than a port so this file needs
no third-party module, matching every other smoke here.
"""

from __future__ import annotations

import json
import os
import selectors
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

TOKEN = "smoke-token-that-is-long-enough-for-the-bearer-rule"
SESSION_ID = "browser-smoke"
# Typed into the shell, then read back off the screen. Distinct enough that it
# cannot be confused with a prompt, a path, or anything the shell prints itself.
MARKER = "afterminal-browser-smoke-9f3a"

STUB_BROWSER = """#!/bin/sh
# AFUI launches this as the window and treats its exit as the person closing it,
# so it records the URL and then waits: the session must outlive the launch.
printf '%s\n' "$@" > "$AFTERMINAL_BROWSER_URL_FILE"
while true; do sleep 1; done
"""


def read_events(process: subprocess.Popen[str], phase: str, timeout: float) -> dict:
    if process.stdout is None:
        raise RuntimeError("afterminal stdout is unavailable")
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    deadline = time.monotonic() + timeout
    lines: list[str] = []
    while time.monotonic() < deadline:
        if process.poll() is not None and not selector.select(timeout=0):
            raise RuntimeError(
                f"afterminal exited before {phase} ({process.returncode}): {' | '.join(lines)}"
            )
        for key, _ in selector.select(timeout=0.25):
            line = key.fileobj.readline()
            if not line:
                continue
            lines.append(line.rstrip())
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            progress = event.get("progress", {})
            if event.get("kind") == "progress" and progress.get("phase") == phase:
                return progress
    raise RuntimeError(f"timed out waiting for {phase}: {' | '.join(lines)}")


def api_get(api_url: str, path: str) -> tuple[int, dict]:
    request = urllib.request.Request(
        f"{api_url.rstrip('/')}{path}", headers={"Authorization": f"Bearer {TOKEN}"}
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as error:
        return error.code, {}


class Chromium:
    """A headless Chromium driven over CDP's pipe transport."""

    def __init__(self, url: str, profile: str, binary: str,
                 window_size: tuple[int, int] | None = None) -> None:
        to_child_r, to_child_w = os.pipe()
        from_child_r, from_child_w = os.pipe()
        # os.pipe() hands out the lowest free descriptors, which are 3 and 4 —
        # exactly the numbers Chromium wants. Move the ends this process keeps
        # before claiming them, or dup2 closes the write end over itself.
        self._write = os.dup(to_child_w)
        self._read = os.dup(from_child_r)
        os.close(to_child_w)
        os.close(from_child_r)
        # Descriptors 3 and 4 are not ours to take: by this point afterminal's
        # own stdout pipe may be sitting on one of them, and closing it after
        # the spawn would cut the event stream this test still reads. Save
        # whatever is there, borrow the numbers, and put it back.
        self._saved: dict[int, int | None] = {}
        for fd in (3, 4):
            try:
                self._saved[fd] = os.dup(fd)
            except OSError:
                self._saved[fd] = None
        os.dup2(to_child_r, 3)
        os.dup2(from_child_w, 4)
        os.set_inheritable(3, True)
        os.set_inheritable(4, True)
        # `--window-size` at launch is the only size this honours: headless
        # ignores a later resize, and a device-metrics override leaves the page's
        # own height alone, so both read the same number for a tall viewport and
        # a short one.
        sizing = [f"--window-size={window_size[0]},{window_size[1]}"] if window_size else []
        self.process = subprocess.Popen(
            [
                binary,
                "--headless=new",
                *sizing,
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-gpu",
                f"--user-data-dir={profile}",
                url,
            ],
            pass_fds=(3, 4),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for fd, saved in self._saved.items():
            if saved is None:
                os.close(fd)
            else:
                os.dup2(saved, fd)
                os.close(saved)
        os.close(to_child_r)
        os.close(from_child_w)
        self._buffer = b""
        self._next_id = 0

    def call(self, method: str, params: dict | None = None, session: str | None = None,
             timeout: float = 20.0) -> dict:
        self._next_id += 1
        message: dict = {"id": self._next_id, "method": method, "params": params or {}}
        if session:
            message["sessionId"] = session
        os.write(self._write, json.dumps(message).encode() + b"\0")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            while b"\0" in self._buffer:
                raw, self._buffer = self._buffer.split(b"\0", 1)
                reply = json.loads(raw)
                if reply.get("id") != message["id"]:
                    continue  # an event, or another call's answer
                if "error" in reply:
                    raise RuntimeError(f"{method} failed: {reply['error']}")
                return reply.get("result", {})
            self._buffer += os.read(self._read, 65536)
        raise RuntimeError(f"timed out waiting for {method}")

    def attach_to_page(self, timeout: float = 30.0, with_target: bool = False):
        """The page's CDP session, and its target id when the caller needs it.

        A caller that resizes the window needs the target to ask which window
        the page is in.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for target in self.call("Target.getTargets").get("targetInfos", []):
                if target.get("type") == "page":
                    session = self.call(
                        "Target.attachToTarget",
                        {"targetId": target["targetId"], "flatten": True},
                    )["sessionId"]
                    return (target["targetId"], session) if with_target else session
            time.sleep(0.25)
        raise RuntimeError("no page target appeared")

    def type_text(self, session: str, text: str) -> None:
        for char in text:
            for kind in ("keyDown", "keyUp"):
                params: dict = {"type": kind, "key": char}
                if kind == "keyDown":
                    params["text"] = char
                self.call("Input.dispatchKeyEvent", params, session=session)

    def press_enter(self, session: str) -> None:
        for kind in ("rawKeyDown", "char", "keyUp"):
            self.call(
                "Input.dispatchKeyEvent",
                {
                    "type": kind,
                    "key": "Enter",
                    "code": "Enter",
                    "windowsVirtualKeyCode": 13,
                    "text": "\r",
                },
                session=session,
            )

    def close(self) -> None:
        try:
            self.process.terminate()
            self.process.wait(timeout=15)
        except Exception:
            self.process.kill()
        for fd in (self._read, self._write):
            try:
                os.close(fd)
            except OSError:
                pass


def find_chromium() -> str | None:
    candidates = [
        os.environ.get("AFTERMINAL_BROWSER_SMOKE_BINARY"),
        "/opt/homebrew/bin/chromium",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ]
    for candidate in candidates:
        if candidate and os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    return None


def click_element(chromium: "Chromium", page: str, selector: str) -> bool:
    """Click an element the way a finger does, by its place on screen."""
    box = chromium.call(
        "Runtime.evaluate",
        {"expression": f"(() => {{ const node = document.querySelector({selector!r});"
                       " if (!node) return null; const r = node.getBoundingClientRect();"
                       " return JSON.stringify({x: r.x + r.width / 2,"
                       " y: r.y + r.height / 2}); })()",
         "returnByValue": True},
        session=page,
    )["result"]["value"]
    if not box:
        return False
    at = json.loads(box)
    for kind in ("mousePressed", "mouseReleased"):
        chromium.call("Input.dispatchMouseEvent",
                      {"type": kind, "x": at["x"], "y": at["y"], "button": "left",
                       "clickCount": 1}, session=page)
    return True


def await_line(api_url: str, expected: str, complaint: str, timeout: float = 25.0) -> None:
    """Wait for `expected` as a whole line of output.

    A substring would be satisfied by the very failure this is looking for: two
    commands running into each other still contain both of their texts.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        lines = [line.strip() for line in screen_text(api_url, SESSION_ID).splitlines()]
        if expected in lines:
            return
        time.sleep(0.4)
    visible = " / ".join(
        line.strip() for line in screen_text(api_url, SESSION_ID).splitlines() if line.strip()
    )
    raise RuntimeError(f"{complaint}: never saw {expected!r} as a line. Screen held: {visible}")


def screen_text(api_url: str, session_id: str) -> str:
    status, screen = api_get(api_url, f"/v1/sessions/{session_id}/screen")
    if status != 200 or not isinstance(screen, dict):
        raise RuntimeError(f"screen request failed: {status} {screen}")
    # The API answers in an AFDATA envelope, and a screen is a grid of cells
    # rather than lines: each cell carries its own text and attributes, which is
    # what makes it authoritative rather than a rendering.
    rows = screen.get("result", screen).get("cells")
    if not isinstance(rows, list):
        raise RuntimeError(f"screen has no cells: {json.dumps(screen)[:200]}")
    rendered: list[str] = []
    for row in rows:
        if not isinstance(row, list):
            continue
        rendered.append("".join(
            cell.get("text", "") for cell in row if isinstance(cell, dict)
        ))
    return "\n".join(rendered)


def main() -> int:
    if len(sys.argv) != 2:
        raise RuntimeError("usage: ui_browser_smoke.py PATH_TO_AFTERMINAL")
    binary = sys.argv[1]

    browser = find_chromium()
    if browser is None:
        # Skipping silently would let this file report success on a machine that
        # never ran a browser, which is the failure mode it exists to prevent.
        print(
            "ui_browser_smoke: no Chromium found; set AFTERMINAL_BROWSER_SMOKE_BINARY",
            file=sys.stderr,
        )
        return 2

    with tempfile.TemporaryDirectory(prefix="afterminal-browser-smoke-") as workspace:
        stub_path = os.path.join(workspace, "stub-browser")
        url_file = os.path.join(workspace, "window-url")
        profile = os.path.join(workspace, "chrome-profile")
        os.makedirs(profile, exist_ok=True)
        with open(stub_path, "w", encoding="utf-8") as handle:
            handle.write(STUB_BROWSER)
        os.chmod(stub_path, os.stat(stub_path).st_mode | stat.S_IEXEC | stat.S_IXGRP)

        environment = os.environ.copy()
        environment["AFTERMINAL_API_ACCESS_TOKEN_SECRET"] = TOKEN
        environment["AFUI_BROWSER_BINARY"] = stub_path
        environment["AFTERMINAL_BROWSER_URL_FILE"] = url_file

        process = subprocess.Popen(
            [binary, "ui", SESSION_ID, "--port", "0", "--program", "/bin/sh",
             "--mode", "window"],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        chromium: Chromium | None = None
        try:
            ready = read_events(process, "ui_ready", 30)
            api_url = ready.get("api_url")
            if not isinstance(api_url, str):
                raise RuntimeError(f"unexpected ui_ready event: {ready}")

            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and not os.path.exists(url_file):
                time.sleep(0.1)
            if not os.path.exists(url_file):
                raise RuntimeError("AFUI never launched the window")
            # AFUI launches a browser the way a browser expects, so the URL
            # arrives inside an argument (`--app=<url>`) rather than alone.
            with open(url_file, encoding="utf-8") as handle:
                arguments = handle.read().split()
            window_url = next(
                (
                    argument.split("=", 1)[1] if argument.startswith("--app=") else argument
                    for argument in arguments
                    if argument.startswith("http") or argument.startswith("--app=http")
                ),
                "",
            )
            if not window_url.startswith("http"):
                raise RuntimeError(f"no URL among the browser arguments: {arguments!r}")

            chromium = Chromium(window_url, profile, browser)
            page = chromium.attach_to_page()
            chromium.call("Runtime.enable", session=page)

            # A keystroke means nothing until the page has drawn the session and
            # opened its own connection, so wait for the screen the runtime
            # renders rather than for a timer. `#terminal` is the node app.js
            # binds to; the textarea it types through is created at runtime and
            # focused when a session is selected.
            deadline = time.monotonic() + 40
            while time.monotonic() < deadline:
                result = chromium.call(
                    "Runtime.evaluate",
                    {
                        "expression": "!!document.querySelector('#terminal .terminal-screen')",
                        "returnByValue": True,
                    },
                    session=page,
                )
                if result.get("result", {}).get("value") is True:
                    break
                time.sleep(0.25)
            else:
                raise RuntimeError("the terminal page never rendered a session screen")

            # Click the terminal the way a person does; that is what hands focus
            # to the textarea the input listeners live on.
            box = chromium.call(
                "Runtime.evaluate",
                {
                    "expression": "(() => { const r = document.querySelector('#terminal')"
                                  ".getBoundingClientRect();"
                                  " return JSON.stringify({x: r.x + r.width / 2,"
                                  " y: r.y + r.height / 2}); })()",
                    "returnByValue": True,
                },
                session=page,
            )["result"]["value"]
            centre = json.loads(box)
            for kind in ("mousePressed", "mouseReleased"):
                chromium.call(
                    "Input.dispatchMouseEvent",
                    {"type": kind, "x": centre["x"], "y": centre["y"],
                     "button": "left", "clickCount": 1},
                    session=page,
                )

            # `Input.insertText` is the path this page listens on: it fires
            # beforeinput and input on the focused textarea, which is what the
            # IME bridge reads. Enter goes through keydown, where SPECIAL_KEYS
            # turns it into the byte the PTY expects.
            chromium.call("Input.insertText", {"text": f"echo {MARKER}"}, session=page)
            # A pause a person would take. CDP can deliver Return about a
            # millisecond after the text, before the page has even seen the
            # input event, and nothing can order what has not arrived.
            time.sleep(0.2)
            chromium.press_enter(page)
            await_line(api_url, MARKER, "a real key event in the page did not become PTY input")

            # Enter is pressed immediately after the text, with no pause. Text is
            # batched behind a timer and an action is not, so an action that does
            # not flush first overtakes the line it belongs to: the shell ran an
            # empty prompt and the command landed on the next one, joined to
            # whatever was typed after it.
            # Committing a composition queues its text from inside the
            # compositionend handler, and a person presses Return straight
            # afterwards — inside the 8ms window the text batch waits out. An
            # action that does not flush that batch first reaches the PTY ahead
            # of its own line, so the shell runs an empty prompt and the command
            # lands on the next one. No pause here on purpose: the pause is what
            # hides this.
            chromium.call("Input.imeSetComposition",
                          {"text": "pai", "selectionStart": 3, "selectionEnd": 3},
                          session=page)
            time.sleep(0.25)
            chromium.call("Input.insertText", {"text": "echo ordering-holds"}, session=page)
            chromium.press_enter(page)
            await_line(api_url, "ordering-holds",
                       "Return overtook the line committed just before it")

            # A composition, then the first keystroke after it. Chromium fires no
            # input event after compositionend, so a flag set at commit time and
            # cleared by the next input event ate that keystroke — on a Chinese
            # keyboard, the space between two words.
            chromium.call("Input.imeSetComposition",
                          {"text": "ni", "selectionStart": 2, "selectionEnd": 2},
                          session=page)
            time.sleep(0.25)
            chromium.call("Input.insertText", {"text": "echo 你好"}, session=page)
            time.sleep(0.35)
            chromium.call("Input.insertText", {"text": " "}, session=page)
            time.sleep(0.2)
            chromium.call("Input.insertText", {"text": "world"}, session=page)
            time.sleep(0.2)
            chromium.press_enter(page)
            await_line(api_url, "你好 world",
                         "the keystroke after a composition was swallowed")

            # An IME anchors its candidate window to the focused field's caret,
            # so the field has to be on the cursor: parked at the terminal's
            # corner, the candidate list sat in the corner too and the half-typed
            # word appeared nowhere. Geometry and visibility are checkable here;
            # where macOS actually draws the candidate window is not.
            chromium.call("Input.imeSetComposition",
                          {"text": "wei zhi", "selectionStart": 7, "selectionEnd": 7},
                          session=page)
            time.sleep(0.3)
            anchored = json.loads(chromium.call(
                "Runtime.evaluate",
                {"expression": "(() => {"
                               " const field = document.querySelector('.terminal-input');"
                               " const cursor = document.querySelector("
                               "'.terminal-cell[data-cursor=\"true\"]');"
                               " if (!cursor) return JSON.stringify({error: 'no cursor cell'});"
                               " const a = field.getBoundingClientRect();"
                               " const b = cursor.getBoundingClientRect();"
                               " return JSON.stringify({dx: Math.abs(a.left - b.left),"
                               " dy: Math.abs(a.top - b.top),"
                               " opacity: Number(getComputedStyle(field).opacity),"
                               " preedit: field.value}); })()",
                 "returnByValue": True},
                session=page,
            )["result"]["value"])
            if anchored.get("error"):
                raise RuntimeError(f"could not find the cursor cell: {anchored}")
            if anchored["dx"] > 2 or anchored["dy"] > 2:
                raise RuntimeError(
                    "the composition field is not on the cursor, so an IME will anchor "
                    f"its candidate window somewhere else: {anchored}"
                )
            if anchored["opacity"] < 0.9 or not anchored["preedit"]:
                raise RuntimeError(f"the half-typed word is not visible at the cursor: {anchored}")
            chromium.call("Input.insertText", {"text": "echo 位置"}, session=page)
            time.sleep(0.3)
            hidden = chromium.call(
                "Runtime.evaluate",
                {"expression": "Number(getComputedStyle("
                               "document.querySelector('.terminal-input')).opacity)",
                 "returnByValue": True},
                session=page,
            )["result"]["value"]
            if hidden > 0.5:
                raise RuntimeError("the composition field stayed visible after the commit")
            chromium.press_enter(page)
            await_line(api_url, "位置", "the committed composition never reached the PTY")

            # Backspace right after a character, which is the same ordering
            # question asked by a key that edits rather than commits.
            chromium.call("Input.insertText", {"text": "echo 甲乙"}, session=page)
            time.sleep(0.25)
            for kind in ("rawKeyDown", "keyUp"):
                chromium.call(
                    "Input.dispatchKeyEvent",
                    {"type": kind, "key": "Backspace", "code": "Backspace",
                     "windowsVirtualKeyCode": 8},
                    session=page,
                )
            time.sleep(0.2)
            chromium.press_enter(page)
            await_line(api_url, "甲", "Backspace did not delete the character before it")

            # Output has to stay selectable while the screen keeps refreshing.
            # Replacing the whole grid every frame tore the selection out within
            # a second or two, so output could be read and never copied.
            # Start from a clean screen with one line on it. Selecting the
            # topmost line of a long history made this flaky: enough output and
            # the screen scrolls, which rewrites that row for a real reason and
            # takes the selection with it — a true failure of a test that meant
            # to ask something else.
            chromium.call("Input.insertText", {"text": "clear"}, session=page)
            time.sleep(0.2)
            chromium.press_enter(page)
            time.sleep(1.0)
            chromium.call("Input.insertText", {"text": "echo hold-this-line"}, session=page)
            time.sleep(0.2)
            chromium.press_enter(page)
            await_line(api_url, "hold-this-line", "the line to select never appeared")
            # await_line returns when the text lands, which is before the prompt
            # after it has been drawn. Selecting into a screen that is still
            # settling means the next frame rewrites the row under the selection
            # for a real reason, and this check is about an idle screen.
            time.sleep(1.5)

            # Select by dragging, the way a person does, and release. Setting a
            # Range from script skips the click that ends a real drag — and that
            # click was the thing throwing the selection away, so a script-made
            # selection passed this check while the product was unusable.
            geometry = json.loads(chromium.call(
                "Runtime.evaluate",
                {"expression": "(() => {"
                               " const cells = [...document.querySelectorAll("
                               "'#terminal .terminal-cell')].filter(n => n.textContent.trim());"
                               " const first = cells[0].getBoundingClientRect();"
                               " const last = cells[10].getBoundingClientRect();"
                               " return JSON.stringify({x1: first.x + 1,"
                               " y1: first.y + first.height / 2,"
                               " x2: last.x + last.width - 1,"
                               " y2: last.y + last.height / 2}); })()",
                 "returnByValue": True},
                session=page,
            )["result"]["value"])
            chromium.call("Input.dispatchMouseEvent",
                          {"type": "mousePressed", "x": geometry["x1"], "y": geometry["y1"],
                           "button": "left", "clickCount": 1}, session=page)
            chromium.call("Input.dispatchMouseEvent",
                          {"type": "mouseMoved", "x": geometry["x2"], "y": geometry["y2"],
                           "button": "left"}, session=page)
            during = chromium.call(
                "Runtime.evaluate",
                {"expression": "getSelection().toString().length", "returnByValue": True},
                session=page,
            )["result"]["value"]
            chromium.call("Input.dispatchMouseEvent",
                          {"type": "mouseReleased", "x": geometry["x2"], "y": geometry["y2"],
                           "button": "left", "clickCount": 1}, session=page)
            held = chromium.call(
                "Runtime.evaluate",
                {"expression": "getSelection().toString().length", "returnByValue": True},
                session=page,
            )["result"]["value"]
            if not held:
                raise RuntimeError(
                    "the selection was gone as soon as the mouse came up: the click "
                    f"ending a drag moved focus and collapsed it (selected {during} "
                    f"while dragging, {held} after release, over {geometry})"
                )
            time.sleep(4)
            still = chromium.call(
                "Runtime.evaluate",
                {"expression": "getSelection().toString().length", "returnByValue": True},
                session=page,
            )["result"]["value"]
            if not still:
                raise RuntimeError(
                    "the selection was gone after the screen refreshed: output that "
                    f"cannot be held cannot be copied (held {held} on release, {still} "
                    "four seconds later)"
                )


            # Ctrl from the key bar, then a letter: the phone path to a signal,
            # which has no keyboard shortcut to fall back on.
            chromium.call("Input.insertText", {"text": "sleep 30"}, session=page)
            time.sleep(0.2)
            chromium.press_enter(page)
            time.sleep(1.0)
            click_element(chromium, page, "[data-key='ctrl']")
            time.sleep(0.3)
            chromium.call("Input.insertText", {"text": "c"}, session=page)
            time.sleep(1.2)
            chromium.call("Input.insertText", {"text": "echo interrupted-ok"}, session=page)
            time.sleep(0.2)
            chromium.press_enter(page)
            await_line(api_url, "interrupted-ok",
                       "Ctrl from the key bar did not interrupt the running program")

            # Secret input publishes nothing derived from what is typed. Asserted
            # against the authoritative screen rather than the page, because the
            # page could hide it and the runtime still hand it to another actor.
            secret = "hunter2-must-not-appear"
            if not click_element(chromium, page, "#secret-input"):
                raise RuntimeError("the private-input control is missing from the page")
            time.sleep(0.8)
            chromium.call("Input.insertText", {"text": secret}, session=page)
            time.sleep(1.0)
            if secret in screen_text(api_url, SESSION_ID):
                raise RuntimeError(
                    "what was typed in private input reached the session screen"
                )

            # Only a person may end it, and ending waits for the session to fall
            # quiet so the echo of what was just typed is not released as
            # publishing resumes. The screen has to come back, without it.
            if not click_element(chromium, page, "#secret-input"):
                raise RuntimeError("the private-input control vanished while it was on")
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                rows = chromium.call(
                    "Runtime.evaluate",
                    {"expression": "document.querySelectorAll('#terminal .terminal-row').length",
                     "returnByValue": True},
                    session=page,
                )["result"]["value"]
                if rows:
                    break
                time.sleep(0.4)
            else:
                raise RuntimeError("the screen never came back after private input ended")
            if secret in screen_text(api_url, SESSION_ID):
                raise RuntimeError(
                    "the secret was released onto the screen when publishing resumed"
                )


            print(f"ui_browser_smoke: {browser} typed into the PTY, kept input in order "
                  f"through a composition and a Backspace, and held a selection across "
                  f"{still} characters of refreshing output")
            return 0
        finally:
            if chromium is not None:
                chromium.close()
            process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
            # The stub browser waits forever by design — its lifetime is the
            # window's — and it is AFUI's child rather than this process's, so
            # ending afterminal does not reap it. Left alone it outlives the run
            # and every earlier run, which is how a machine ends up with a dozen
            # spinning shells.
            subprocess.run(["pkill", "-f", stub_path], check=False,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    sys.exit(main())
