#!/usr/bin/env python3
"""Prove a UI launched inside a remote terminal follows it into session mode."""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import smoke_support  # noqa: E402
import time
import urllib.request
from pathlib import Path


TOKEN = "terminal-delivery-smoke-0123456789-abcd"


def read_ready(process: subprocess.Popen[str], timeout: float) -> dict:
    reader = smoke_support.LineReader(process)
    deadline = time.monotonic() + timeout
    lines: list[str] = []
    while time.monotonic() < deadline:
        line = reader.next_line(0.25)
        if line is None:
            if process.poll() is not None:
                break
            continue
        lines.append(line.rstrip())
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        progress = event.get("progress", {})
        if event.get("kind") == "progress" and progress.get("phase") == "ui_ready":
            return progress
    raise RuntimeError(f"outer terminal never became ready: {' | '.join(lines)}")


def registry_entries(config_dir: Path) -> list[dict]:
    entries = []
    for path in (config_dir / "sessions").glob("*.json"):
        try:
            entries.append(json.loads(path.read_text(encoding="utf-8")))
        except (OSError, json.JSONDecodeError):
            continue
    return entries


def wait_for_subjects(config_dir: Path, wanted: set[str], timeout: float) -> dict[str, dict]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        by_subject = {entry.get("subject"): entry for entry in registry_entries(config_dir)}
        if wanted <= by_subject.keys():
            return by_subject
        time.sleep(0.05)
    raise RuntimeError(
        f"nested session never registered; found {registry_entries(config_dir)!r}"
    )


def end(entry: dict) -> None:
    access_url = entry.get("access_url_secret")
    if not isinstance(access_url, str):
        raise RuntimeError(f"registry entry has no access URL: {entry!r}")
    request = urllib.request.Request(
        f"{access_url.rstrip('/')}/__afui/end", data=b"", method="POST"
    )
    with urllib.request.urlopen(request, timeout=5) as response:
        if response.status != 204:
            raise RuntimeError(f"session end answered {response.status}")


def main() -> int:
    if len(sys.argv) != 2:
        raise RuntimeError("usage: ui_delivery_smoke.py PATH_TO_AFTERMINAL")
    binary = os.path.abspath(sys.argv[1])
    with tempfile.TemporaryDirectory(prefix="afterminal-delivery-smoke-") as temporary:
        workspace = Path(temporary)
        config_dir = workspace / "afui-config"
        browser_called = workspace / "browser-called"
        browser = smoke_support.write_marker_launcher(
            workspace, "unexpected-browser", browser_called
        )

        launch_inner = smoke_support.write_command_launcher(
            workspace,
            "launch-inner",
            [
                binary,
                "ui",
                "inner",
                "--port",
                "0",
                "--program",
                smoke_support.shell_program(),
                "--title",
                "inherited-inner",
            ],
        )
        environment = os.environ.copy()
        environment.update(
            {
                "AFTERMINAL_API_ACCESS_TOKEN_SECRET": TOKEN,
                "AFUI_BROWSER_BINARY": browser,
                "AFUI_CONFIG_DIR": str(config_dir),
            }
        )
        environment.pop("AFUI_NO_REGISTRY", None)
        process = subprocess.Popen(
            [
                binary,
                "ui",
                "outer",
                "--mode",
                "session",
                "--port",
                "0",
                "--program",
                launch_inner,
                "--title",
                "inherited-outer",
            ],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        try:
            ready = read_ready(process, 20)
            if ready.get("mode") != "session" or ready.get("link_url") is not None:
                raise RuntimeError(f"unexpected outer ready event: {ready!r}")
            entries = wait_for_subjects(
                config_dir, {"inherited-outer", "inherited-inner"}, 20
            )
            if browser_called.exists():
                raise RuntimeError(
                    "the nested UI opened a local browser instead of inheriting session delivery"
                )

            end(entries["inherited-inner"])
            end(entries["inherited-outer"])
            if process.wait(timeout=20) != 0:
                raise RuntimeError(f"outer afterminal exited {process.returncode}")
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
