#!/usr/bin/env python3
"""Measure how much terminal is left at phone viewports.

Separate from `ui_browser_smoke.py` on purpose. That file types, composes,
selects and toggles private input, and by the time it finished a viewport
override no longer reached the page: landscape and landscape-with-a-keyboard
both measured 350px, the same number, so the check was measuring nothing. On a
page that has only just loaded the override lands and the three viewports differ
as they should.

What this guards: every responsive rule on this page keys off width, and a phone
in landscape is wider than all of them, so it was handed the desktop layout at a
height no desktop has. Measured before the fix, in rows of terminal:

    portrait 390x844                28
    landscape 844x390               11
    landscape + keyboard 844x200     1

One row is what "not even a single line" means, and it is what a person reported.
"""

from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from ui_browser_smoke import (  # noqa: E402
    Chromium,
    SESSION_ID,
    STUB_BROWSER,
    TOKEN,
    find_chromium,
    read_events,
)

# A phone, sideways, and sideways with a keyboard over half of it. The floors are
# set well under what a correct layout gives, because this is here to catch a
# collapse rather than to pin a number that a font change may move.
# Two shapes, each in its own browser because the size can only be set at
# launch. A third case for a keyboard covering half the screen is missing on
# purpose: headless clamps a window's height at around 375px, so it measured the
# same as landscape, and a case that cannot come out differently is not a case.
VIEWPORTS = (
    ("portrait", 390, 844, 25),
    ("landscape", 844, 390, 14),
)


def main() -> int:
    if len(sys.argv) != 2:
        raise RuntimeError("usage: ui_viewport_smoke.py PATH_TO_AFTERMINAL")
    binary = sys.argv[1]
    browser = find_chromium()
    if browser is None:
        print(
            "ui_viewport_smoke: no Chromium found; set AFTERMINAL_BROWSER_SMOKE_BINARY",
            file=sys.stderr,
        )
        return 2

    with tempfile.TemporaryDirectory(prefix="afterminal-viewport-smoke-") as workspace:
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
            read_events(process, "ui_ready", 30)
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and not os.path.exists(url_file):
                time.sleep(0.1)
            if not os.path.exists(url_file):
                raise RuntimeError("AFUI never launched the window")
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

            measured = []
            for label, width, height, least in VIEWPORTS:
                chromium = Chromium(
                    window_url,
                    os.path.join(profile, f"{width}x{height}"),
                    browser,
                    window_size=(width, height),
                )
                page = chromium.attach_to_page()
                chromium.call("Runtime.enable", session=page)
                deadline = time.monotonic() + 40
                while time.monotonic() < deadline:
                    drawn = chromium.call(
                        "Runtime.evaluate",
                        {"expression": "!!document.querySelector('#terminal .terminal-row')",
                         "returnByValue": True},
                        session=page,
                    )["result"]["value"]
                    if drawn is True:
                        break
                    time.sleep(0.25)
                else:
                    raise RuntimeError(f"{label}: the terminal page never drew a row")

                # The runtime resizes the PTY to the new shape and sends a screen
                # back, so the panel passes through intermediate heights; wait for
                # two readings that agree.
                previous = None
                shape = {"rows": 0, "height": 0}
                settle = time.monotonic() + 15
                while time.monotonic() < settle:
                    time.sleep(0.5)
                    shape = json.loads(chromium.call(
                        "Runtime.evaluate",
                        # `#terminal` grows with its content, so measuring it
                        # counts rows of output rather than room for them. The
                        # panel is the space the layout actually leaves.
                        {"expression": "(() => {"
                                       " const panel = document.querySelector('.terminal-panel');"
                                       " const row = document.querySelector('.terminal-row');"
                                       " const rowHeight = row ?"
                                       " row.getBoundingClientRect().height : 0;"
                                       " const height = panel.getBoundingClientRect().height;"
                                       " return JSON.stringify({rows: rowHeight ?"
                                       " Math.floor(height / rowHeight) : 0,"
                                       " height: Math.round(height)}); })()",
                         "returnByValue": True},
                        session=page,
                    )["result"]["value"])
                    if shape["rows"] and shape == previous:
                        break
                    previous = shape

                measured.append(f"{label} {shape['rows']} rows in {shape['height']}px")
                if shape["rows"] < least:
                    raise RuntimeError(
                        f"{label} left {shape['rows']} rows of terminal in "
                        f"{shape['height']}px, fewer than the {least} this size should "
                        f"hold: the chrome is taking height the terminal needs"
                    )
                chromium.close()
                chromium = None
            print("ui_viewport_smoke: " + "; ".join(measured))
            return 0
        finally:
            if chromium is not None:
                chromium.close()
            process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
            subprocess.run(["pkill", "-f", stub_path], check=False,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    sys.exit(main())
