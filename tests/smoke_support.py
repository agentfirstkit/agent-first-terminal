"""Platform-portable pieces shared by afterminal's smoke tests.

afterminal ships a Windows binary, so the smokes that drive the real binary have
to be able to run there. Three things stood between them and that, and all three
live here rather than in each smoke: a readiness wait built on `selectors`,
which on Windows only accepts sockets and fails a pipe with `WinError 10038`; a
session program spelled `/bin/sh`; and a launcher stub written as a `#!/bin/sh`
script, which Windows has no way to execute.
"""

from __future__ import annotations

import os
import queue
import stat
import subprocess
import sys
import time
import threading
from pathlib import Path

WINDOWS = sys.platform == "win32"


class LineReader:
    """Reads a process's lines on a thread, so a waiter can use a deadline.

    This is what `selectors` was doing, minus the part that only works on Unix:
    readiness on a pipe is not something Windows' selector can answer, but a
    blocking read on a thread is the same question asked in a portable way.
    """

    def __init__(self, process: subprocess.Popen[str]) -> None:
        if process.stdout is None:
            raise RuntimeError("process stdout is unavailable")
        self._lines: queue.Queue[str | None] = queue.Queue()
        self._thread = threading.Thread(
            target=self._pump, args=(process.stdout,), daemon=True
        )
        self._thread.start()

    def _pump(self, stream: object) -> None:
        try:
            for line in stream:  # type: ignore[attr-defined]
                self._lines.put(line)
        finally:
            self._lines.put(None)

    def next_line(self, timeout: float) -> str | None:
        """One line, or None if none arrived before `timeout` (or on EOF)."""
        try:
            return self._lines.get(timeout=max(timeout, 0.0))
        except queue.Empty:
            return None

    def drain(self, timeout: float = 2.0) -> list[str]:
        """Every line still queued, up to end of output.

        Once a reader owns the stream, reading the process's remaining output
        has to go through it — `process.stdout.read()` would find that the
        thread had already taken everything.
        """
        collected: list[str] = []
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return collected
            line = self.next_line(remaining)
            if line is None:
                return collected
            collected.append(line.rstrip("\r\n"))


def shell_program() -> str:
    """A shell the smokes can hand to `--program`, per platform."""
    if WINDOWS:
        return os.environ.get("COMSPEC", "cmd.exe")
    return "/bin/sh"


def shell_args() -> list[str]:
    """Arguments that start that shell interactively.

    Windows enables delayed expansion so an unset variable expands to nothing
    rather than to its own name, which is what lets a report read the same on
    both platforms.
    """
    if WINDOWS:
        return ["/V:ON"]
    return []


def write_python_launcher(directory: Path, name: str, source: str) -> str:
    """Write a Python launcher and return the path to hand to a caller.

    A `#!` line is how a script says how to run itself on Unix and nothing at
    all on Windows, so there the script gets a batch wrapper that names the
    interpreter. Arguments are forwarded, because these stand in for a browser
    AFUI invokes with a URL.
    """
    script = directory / f"{name}.py"
    if WINDOWS:
        script.write_text(source, encoding="utf-8")
        wrapper = directory / f"{name}.bat"
        wrapper.write_text(
            f'@echo off\r\n"{sys.executable}" "{script}" %*\r\n',
            encoding="utf-8",
        )
        return str(wrapper)

    script.write_text(source, encoding="utf-8")
    script.chmod(script.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP)
    return str(script)


def write_command_launcher(directory: Path, name: str, command: list[str]) -> str:
    """Write a launcher that runs one fixed command, forwarding nothing."""
    if WINDOWS:
        wrapper = directory / f"{name}.bat"
        wrapper.write_text(
            "@echo off\r\n" + subprocess.list2cmdline(command) + "\r\n",
            encoding="utf-8",
        )
        return str(wrapper)

    import shlex

    script = directory / name
    script.write_text(
        "#!/bin/sh\nexec " + " ".join(shlex.quote(part) for part in command) + "\n",
        encoding="utf-8",
    )
    script.chmod(script.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP)
    return str(script)


def write_marker_launcher(directory: Path, name: str, marker: Path) -> str:
    """Write a launcher that only records that it was called."""
    if WINDOWS:
        wrapper = directory / f"{name}.bat"
        wrapper.write_text(
            f'@echo off\r\ntype nul > "{marker}"\r\n', encoding="utf-8"
        )
        return str(wrapper)

    import shlex

    script = directory / name
    script.write_text(
        f"#!/bin/sh\nset -eu\ntouch {shlex.quote(str(marker))}\n", encoding="utf-8"
    )
    script.chmod(script.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP)
    return str(script)
