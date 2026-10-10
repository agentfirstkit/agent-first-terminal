#!/usr/bin/env python3
"""A failed browser assertion must fail the fixed gate, even inside `if`."""

from pathlib import Path
import subprocess


source = (Path(__file__).resolve().parents[1] / "scripts/test.sh").read_text()
start = source.index("run_ui_browser_smoke() {")
end = source.index("\n}", start) + 2
function = source[start:end]
# Execute the gate's real function with the browser command failing. Building
# the binary is irrelevant to this shell-status regression.
result = subprocess.run(
    [
        "bash", "-c",
        "set -euo pipefail\nROOT_PATH=/unused\n"
        "afdata_run() { return 0; }\n"
        "afdata_log() { :; }\n"
        "python3() { return 1; }\n"
        + function + "\nrun_ui_browser_smoke\n",
    ],
    check=False,
)
if result.returncode != 1:
    raise SystemExit(f"browser failure was lost: gate returned {result.returncode}")
