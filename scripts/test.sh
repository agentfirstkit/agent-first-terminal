#!/usr/bin/env bash
set -euo pipefail

_AFDATA_BASH_SOURCE="$("${AFDATA_BIN:-afdata}" shell bash)"
# shellcheck source=/dev/stdin
source /dev/stdin <<<"$_AFDATA_BASH_SOURCE"
unset _AFDATA_BASH_SOURCE

ROOT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_PATH"

afdata_args_begin "$0 [MODE]"
afdata_args_positional test_mode MODE "Test mode" optional
afdata_args_parse "$@"
MODE="${test_mode:-all}"
TEST_TMP="$(mktemp -d "${TMPDIR:-/tmp}/agent-first-terminal-test.XXXXXX")"
trap 'rm -rf "$TEST_TMP"' EXIT

run_clippy() {
  afdata_run --quiet cargo clippy --all-targets "$@" -- -D warnings
}

# The PTY library and nothing else.
#
# agent-first-ui is banned as a whole crate rather than as its `session` tier,
# and that is exact rather than lazy: afterminal reads a frontend only from the
# `api` build, so a minimal build has no use for even the guard tier. A spore
# that did — afmail, whose template loader calls the guard in every build —
# would ban `axum` and `tokio` here instead and let the crate through.
check_minimal_dependency_tree() {
  local dependency_tree dependency
  dependency_tree="$(cargo tree --no-default-features --edges normal --prefix none)"
  for dependency in agent-first-data agent-first-ui axum base64 clap getrandom reqwest tokio tokio-stream; do
    if [[ "$dependency_tree" == *"$dependency v"* ]]; then
      afdata_error unexpected_dependency \
        "$dependency must not be present in the no-default-features dependency tree"
    fi
  done
}

check_openapi_contract() {
  local binary="$ROOT_PATH/target/debug/afterminal"
  local generated="$TEST_TMP/openapi"

  afdata_run --quiet cargo build --features api --bin afterminal
  afdata_run --quiet "$binary" api export --directory "$generated" --force
  # The whole generated tree is committed: the document, the Schema index, and
  # one file per Schema. A stale Schema file reads as current, so compare the
  # directories rather than the document alone.
  if ! diff -r openapi "$generated" >/dev/null; then
    afdata_error openapi_contract_drift \
      "openapi/ is out of date; run afterminal api export --directory openapi --force"
  fi
  afdata_run --quiet afdata lint openapi/openapi.json
  afdata_run --quiet afdata lint openapi/schemas/index.json
}

check_cli_discovery() {
  local binary="$ROOT_PATH/target/debug/afterminal"

  "$binary" --help --output json >"$TEST_TMP/help.jsonl"
  "$binary" ui --help --output json >"$TEST_TMP/ui-help.jsonl"
  "$binary" --version --output json >"$TEST_TMP/version.jsonl"
  afdata_run --quiet afdata validate "$TEST_TMP/help.jsonl" --strict
  afdata_run --quiet afdata validate "$TEST_TMP/ui-help.jsonl" --strict
  afdata_run --quiet afdata validate "$TEST_TMP/version.jsonl" --strict
}

# The on-screen key bar, driven through the page's own runtime.
#
# A phone cannot reach Ctrl, Esc, Tab or an arrow any other way, so what those
# buttons put on the wire is a product promise rather than a detail — and this
# fails in the local gate rather than only in a browser somebody remembered to
# open. `node` is already required here, so nothing new is.
run_key_bar() {
  afdata_run --quiet node tests/key_bar.mjs
}

run_static() {
  # Parsing is not linting, and this ran neither until now. The pinned version
  # lives in the script itself, mirrored from the monorepo and compared byte for
  # byte there, so this spore and its siblings cannot drift onto different rules.
  "$ROOT_PATH/scripts/install-shellcheck.sh" --check "$ROOT_PATH"
  afdata_run --quiet cargo fmt --all --check
  run_clippy
  run_clippy --all-features
  check_minimal_dependency_tree
  check_openapi_contract
  check_cli_discovery
  afdata_run --quiet node --check src/api/ui/app.js
  run_key_bar
  afdata_run --quiet python3 -m json.tool spore.core.json
}

run_unit() {
  afdata_run --quiet cargo test --all-features --lib --bins --tests
}

run_api_smoke() {
  local binary="$ROOT_PATH/target/debug/afterminal"

  afdata_run --quiet cargo build --features api --bin afterminal
  afdata_run --quiet python3 tests/api_smoke.py "$binary"
}

# Drives the real `afterminal ui` window with a stub browser, so the page and
# every asset it loads are fetched the way the person's window fetches them.
run_ui_smoke() {
  local binary="$ROOT_PATH/target/debug/afterminal"

  afdata_run --quiet cargo build --features api --bin afterminal
  afdata_run --quiet python3 tests/ui_smoke.py "$binary"
}

# Starts a session-delivered terminal whose PTY launches a second `afterminal
# ui` with no mode. The inner UI must inherit `AFUI_DELIVERY=session`, register
# beside the outer one, and never invoke the browser stub.
run_ui_delivery_smoke() {
  local binary="$ROOT_PATH/target/debug/afterminal"

  afdata_run --quiet cargo build --features api --bin afterminal
  afdata_run --quiet python3 tests/ui_delivery_smoke.py "$binary"
}

# Types into the served page from a real headless browser and reads the result
# off the authoritative screen, which is the one thing the stub-browser smokes
# cannot do: their stub never runs the script it downloads.
#
# A machine with no browser is reported, not passed over in silence — a smoke
# that goes quiet on the box where it cannot run is a smoke that never fails.
run_ui_browser_smoke() {
  local binary="$ROOT_PATH/target/debug/afterminal"
  local script status

  afdata_run --quiet cargo build --features api --bin afterminal
  # The second one measures how much terminal a phone-shaped window leaves, and
  # runs a browser per shape because the size can only be set at launch.
  for script in tests/ui_browser_smoke.py tests/ui_viewport_smoke.py; do
    if python3 "$script" "$binary"; then
      continue
    fi
    status=$?
    if [ "$status" -eq 2 ]; then
      afdata_log warn "${script##*/} skipped: no Chromium on this machine"
      continue
    fi
    return "$status"
  done
}

run_minimal() {
  afdata_run --quiet cargo test --no-default-features --lib
}

run_release_smoke() {
  afdata_run --quiet cargo build --release --features api --bin afterminal
  afdata_run --quiet ./scripts/release-smoke.sh target/release
}

case "$MODE" in
  static)        run_static ;;
  unit)          run_unit ;;
  minimal)       run_minimal ;;
  api)           check_openapi_contract; run_unit; run_key_bar; run_api_smoke; run_ui_smoke; run_ui_delivery_smoke; run_ui_browser_smoke ;;
  release-smoke) run_release_smoke ;;
  all)           run_static; run_unit; run_minimal; run_api_smoke; run_ui_smoke; run_ui_delivery_smoke; run_ui_browser_smoke; run_release_smoke ;;
  *)
    afdata_error cli_error "unsupported test mode '$MODE'" \
      "try: $0 static|unit|minimal|api|release-smoke|all" || :
    exit 2
    ;;
esac

afdata_result "agent-first-terminal tests complete [$MODE]"
