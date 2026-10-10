"""Unix shutdown signals must end API/UI commands and revoke attachments."""
import json
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import api_smoke
import smoke_support


def start(binary, args):
    env = os.environ.copy()
    env["AFTERMINAL_API_ACCESS_TOKEN_SECRET"] = api_smoke.TOKEN
    env["AFUI_NO_REGISTRY"] = "1"
    process = subprocess.Popen([binary, *args], env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True)
    reader = smoke_support.LineReader(process)
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        line = reader.next_line(.2)
        if line:
            event = json.loads(line)
            ready = event.get("progress", {})
            if ready.get("phase") in ("api_ready", "ui_ready"):
                return process, reader, ready
        if process.poll() is not None:
            raise RuntimeError(f"exited before ready: {process.returncode}")
    process.kill()
    process.wait()
    raise RuntimeError("shutdown fixture readiness timeout")


def stop(process, reader, signum, attachment=False):
    process.send_signal(signum)
    code = process.wait(timeout=6)
    events = [json.loads(line) for line in reader.drain() if line.startswith("{")]
    if code != 0 or not events or events[-1].get("kind") != "result":
        raise RuntimeError(f"signal {signum}: exit={code}, events={events}")
    result = events[-1]["result"]
    if result.get("stopped") is not True:
        raise RuntimeError(f"missing terminal stopped result: {result}")
    if attachment and result.get("ui_capability_revoked") is not True:
        raise RuntimeError(f"attachment survived shutdown: {result}")


def stop_during_startup(binary, signum):
    """Signal `afterminal ui` before it has reported readiness.

    The initial session's own program writes the pid file, and that session is
    opened before the UI is delivered, so the file appearing is a point inside
    startup that needs no reading of the command's output. Wherever the signal
    then lands relative to `ui_ready`, the command must end with its result and
    take the HUP-immune child with it.
    """
    env = os.environ.copy()
    env["AFTERMINAL_API_ACCESS_TOKEN_SECRET"] = api_smoke.TOKEN
    env["AFUI_NO_REGISTRY"] = "1"
    with tempfile.TemporaryDirectory(prefix="afterminal-shutdown-") as directory:
        pid_file = Path(directory) / "child.pid"
        script = f"trap '' HUP; echo $$ > {shlex.quote(str(pid_file))}; exec sleep 600"
        process = subprocess.Popen(
            [binary, "ui", "startup", "--port", "0", "--mode", "session",
             "--program", "/bin/sh", "--arg=-c", f"--arg={script}"],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        reader = smoke_support.LineReader(process)
        pid = None
        try:
            deadline = time.monotonic() + 15
            while pid is None:
                try:
                    pid = int(pid_file.read_text())
                except (FileNotFoundError, ValueError):
                    if process.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError(
                            f"initial session never started: exit={process.poll()}, "
                            f"output={reader.drain()}")
                    time.sleep(.001)
            stop(process, reader, signum)
            wait_gone(pid, f"startup signal {signum}")
        finally:
            cleanup(process)
            if pid is not None:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass


def wait_gone(pid, context):
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        time.sleep(.02)
    raise RuntimeError(f"HUP-immune child {pid} survived {context}")


def cleanup(process):
    if process.poll() is None:
        process.kill()
        process.wait(timeout=3)


def main():
    if os.name != "posix":
        return 0
    binary = sys.argv[1]
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        for command in (["api", "serve", "--port", "0", "--mode", "local"],
                        ["ui", "--port", "0", "--mode", "session"]):
            process, reader, ready = start(binary, command)
            pid = None
            try:
                api_url = ready["api_url"]
                with tempfile.TemporaryDirectory(prefix="afterminal-shutdown-") as directory:
                    pid_file = Path(directory) / "child.pid"
                    status, result = api_smoke.request(api_url, "POST", "/v1/sessions", {
                        "session_id": "shutdown", "program": "/bin/sh",
                        "args": ["-c", f"trap '' HUP; echo $$ > {shlex.quote(str(pid_file))}; exec sleep 600"],
                    })
                    if status != 200:
                        raise RuntimeError(f"create HUP-immune session: {status} {result}")
                    deadline = time.monotonic() + 3
                    while not pid_file.exists() and time.monotonic() < deadline:
                        time.sleep(.02)
                    pid = int(pid_file.read_text())
                    if command[0] == "api":
                        attached, attached_reader, _ = start(binary, ["ui", "--api-url", api_url, "--mode", "session"])
                        try:
                            stop(attached, attached_reader, signum, attachment=True)
                        finally:
                            cleanup(attached)
                    stop(process, reader, signum)
                    wait_gone(pid, f"signal {signum}")
            finally:
                cleanup(process)
                if pid is not None:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
        # Several rounds per signal: where the signal lands relative to
        # readiness is the one thing this cannot fix, so it samples it.
        for _ in range(5):
            stop_during_startup(binary, signum)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
