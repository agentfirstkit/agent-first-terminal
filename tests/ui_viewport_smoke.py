#!/usr/bin/env python3
"""Measure how much terminal is left at phone viewports.

Separate from `ui_browser_smoke.py` so device metrics are installed on a blank
page before navigation, typing, or IME focus. The requested dimensions are CSS
viewport dimensions, not Chromium window dimensions: a browser window's own
chrome must not reduce the phone viewport this check claims to exercise.

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

# A phone in portrait and landscape. The floors are
# set well under what a correct layout gives, because this is here to catch a
# collapse rather than to pin a number that a font change may move.
# Each shape starts on a blank page, sets device metrics, then navigates. The
# check verifies the actual viewport before measuring terminal room, so a
# browser that ignores emulation cannot silently exercise a different size.
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
                    "about:blank",
                    os.path.join(profile, f"{width}x{height}"),
                    browser,
                    window_size=(width, height),
                )
                page = chromium.attach_to_page()
                chromium.call("Runtime.enable", session=page)
                chromium.call(
                    "Emulation.setDeviceMetricsOverride",
                    {"width": width, "height": height, "deviceScaleFactor": 1,
                     "mobile": False, "screenWidth": width, "screenHeight": height,
                     "screenOrientation": {
                         "type": "landscapePrimary" if width > height else "portraitPrimary",
                         "angle": 90 if width > height else 0,
                     }},
                    session=page,
                )
                chromium.call("Page.navigate", {"url": window_url}, session=page)
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
                        # The stage is the space left after the heading and
                        # key bar; remove the terminal's inner padding too.
                        # Counting the whole panel includes chrome as rows.
                        {"expression": "(() => {"
                                       " const panel = document.querySelector('.terminal-panel');"
                                       " const row = document.querySelector('.terminal-row');"
                                       " const rowHeight = row ?"
                                       " row.getBoundingClientRect().height : 0;"
                                       " const stage = document.querySelector('.terminal-stage');"
                                       " const terminal = getComputedStyle(document.querySelector('#terminal'));"
                                       " const height = stage.getBoundingClientRect().height -"
                                       "   parseFloat(terminal.paddingTop) - parseFloat(terminal.paddingBottom);"
                                       " const workspace = document.querySelector('.workspace');"
                                       " const padding = getComputedStyle(workspace);"
                                       " const bandHeight = selector => {"
                                       "   const node = document.querySelector(selector);"
                                       "   return node ? Math.round(node.getBoundingClientRect().height) : 0;"
                                       " };"
                                       " return JSON.stringify({rows: rowHeight ?"
                                       " Math.floor(height / rowHeight) : 0,"
                                       " height: Math.round(height),"
                                       " panel_height: Math.round(panel.getBoundingClientRect().height),"
                                       " row_height: Math.round(rowHeight * 100) / 100,"
                                       " viewport_width: window.innerWidth,"
                                       " viewport_height: window.innerHeight,"
                                       " visual_viewport_height: window.visualViewport ?"
                                       "   Math.round(window.visualViewport.height) : null,"
                                       " workspace_height: bandHeight('.workspace'),"
                                       " workspace_padding: parseFloat(padding.paddingTop) +"
                                       "   parseFloat(padding.paddingBottom),"
                                       " topbar_height: bandHeight('.topbar'),"
                                       " heading_height: bandHeight('.terminal-heading'),"
                                       " key_bar_height: bandHeight('.key-bar'),"
                                       " terminal_stage_height: bandHeight('.terminal-stage')}); })()",
                         "returnByValue": True},
                        session=page,
                    )["result"]["value"])
                    if shape["rows"] and shape == previous:
                        break
                    previous = shape

                measured.append(f"{label} {shape['rows']} rows in {shape['height']}px")
                if (shape["viewport_width"] != width
                        or shape["viewport_height"] != height
                        or shape["visual_viewport_height"] != height):
                    raise RuntimeError(
                        f"{label}: requested a {width}x{height} CSS viewport but "
                        f"Chromium applied a different size: "
                        f"{json.dumps(shape, sort_keys=True)}"
                    )
                if shape["rows"] < least:
                    raise RuntimeError(
                        f"{label} left {shape['rows']} rows of terminal in "
                        f"{shape['height']}px, fewer than the {least} this size should "
                        f"hold: the chrome is taking height the terminal needs; "
                        f"layout={json.dumps(shape, sort_keys=True)}"
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
