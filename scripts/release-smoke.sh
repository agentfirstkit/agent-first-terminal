#!/usr/bin/env bash
# Check a built afterminal the way a release ships it.
#
# Automated checks prove the feature-gated CLI and both UI deliveries exist in
# the artifact, and tests/ui_browser_smoke.py types into the served page from a
# real browser. What no fixture here can stand in for is a mobile IME, a device
# rotating, or a screen reader; those are release observations, recorded against
# the device matrix the release keeps outside this crate.
set -euo pipefail

_AFDATA_BASH_SOURCE="$("${AFDATA_BIN:-afdata}" shell bash)"
# shellcheck source=/dev/stdin
source /dev/stdin <<<"$_AFDATA_BASH_SOURCE"
unset _AFDATA_BASH_SOURCE

ROOT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_PATH"

afdata_args_begin "$0 [BIN_DIR]"
afdata_args_positional release_bin_dir BIN_DIR "Directory containing the release binary" optional
afdata_args_parse "$@"
BIN_DIR="${release_bin_dir:-target/release}"

AFTERMINAL="$BIN_DIR/afterminal"
if [ ! -x "$AFTERMINAL" ] && [ -x "${AFTERMINAL}.exe" ]; then
  AFTERMINAL="${AFTERMINAL}.exe"
fi
if [ ! -x "$AFTERMINAL" ]; then
  afdata_error release_binary_missing \
    "missing afterminal binary under $BIN_DIR" \
    "build with --features api --bin afterminal before running the release smoke" || :
  exit 1
fi

SMOKE_TMP="$(mktemp -d "${TMPDIR:-/tmp}/afterminal-release-smoke.XXXXXX")"
trap 'rm -rf "$SMOKE_TMP"' EXIT

"$AFTERMINAL" --version --output json >"$SMOKE_TMP/version.jsonl"
"$AFTERMINAL" ui --help --output json >"$SMOKE_TMP/ui-help.jsonl"
"$AFTERMINAL" api --help --output json >"$SMOKE_TMP/api-help.jsonl"
afdata_run --quiet afdata validate "$SMOKE_TMP/version.jsonl" --strict
afdata_run --quiet afdata validate "$SMOKE_TMP/ui-help.jsonl" --strict
afdata_run --quiet afdata validate "$SMOKE_TMP/api-help.jsonl" --strict

# Drive the release binary through the same hermetic API, window and nested
# session flows as the debug gate. The browser is a stub, but every page and
# asset is fetched from the real listener.
afdata_run --quiet python3 tests/api_smoke.py "$AFTERMINAL"
afdata_run --quiet python3 tests/ui_smoke.py "$AFTERMINAL"
afdata_run --quiet python3 tests/ui_delivery_smoke.py "$AFTERMINAL"

afdata_result "afterminal release smoke passed; complete the device matrix on target devices"
