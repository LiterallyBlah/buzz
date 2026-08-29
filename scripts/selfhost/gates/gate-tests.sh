#!/usr/bin/env bash
# =============================================================================
# gate-tests.sh — GATE 1: the test suites are green.
# =============================================================================
# WHAT THIS GATE PROVES
#   The candidate's own unit/integration suites pass on this host, for the
#   crates that build here plus the desktop app:
#     A. cargo check --all-targets  (buzz-core, buzz-sdk, buzz-cli, buzz-acp)
#     B. cargo test                 (same four, --no-fail-fast, doctests included)
#     C. cargo check -p buzz-relay --lib
#     D. pnpm typecheck             (desktop)
#     E. pnpm test                  (desktop)
#     F. gate harness schema contract (canonical convergence path + invariants)
#     G. gate relay-key contract (ephemeral, in-memory, shared candidate path)
#
# WHAT IT DELIBERATELY DOES NOT PROVE
#   * NOT a full-workspace green. See "Known environment gap" below and in
#     gates/README.md: buzz-relay's TEST targets pull openssl through
#     dev-dependencies and this host has no openssl dev headers, so
#     `cargo test --workspace` cannot run here at all. Step C type-checks the
#     relay library so a candidate that does not COMPILE is still caught; it
#     does not run one relay unit test. That coverage has to come from CI.
#   * Nothing about integration behaviour against a live relay — that is
#     gates 2-4.
#   * Nothing about the desktop e2e/Playwright suites (they need a browser
#     stack and a built app; out of scope for a promotion gate of this size).
#
# WAIVERS
#   Test failures are matched against gates/waivers.txt. All-waived => pass with
#   a banner and a downgraded verdict. Any unwaived failure => fail. Compile
#   errors (steps A, C, D) are NEVER waivable — a waiver for "it does not build"
#   is not a waiver, it is a lie.
# =============================================================================
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="gate:tests"
# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"
# shellcheck source=lib/waivers.sh
source "${GATES_DIR}/lib/waivers.sh"

EVIDENCE="${GATES_EVIDENCE:-/tmp/buzz-gates/adhoc/tests}"
WAIVER_FILE="${GATES_WAIVER_FILE:-${GATES_DIR}/waivers.txt}"
SKIP_DESKTOP_TESTS="${GATES_SKIP_DESKTOP_TESTS:-0}"
SKIP_DESKTOP="${GATES_SKIP_DESKTOP:-0}"
SKIP_RUST="${GATES_SKIP_RUST:-0}"

RUST_PKGS=(-p buzz-core -p buzz-sdk -p buzz-cli -p buzz-acp)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --execute)              GATES_EXECUTE=1; shift ;;
    --dry-run)              GATES_EXECUTE=0; shift ;;
    --evidence)             EVIDENCE="$2"; shift 2 ;;
    --waivers)              WAIVER_FILE="$2"; shift 2 ;;
    --skip-desktop)         SKIP_DESKTOP=1; shift ;;
    --skip-desktop-tests)   SKIP_DESKTOP_TESTS=1; shift ;;
    --skip-rust)            SKIP_RUST=1; shift ;;
    *) err "unknown option: $1"; exit 2 ;;
  esac
done

require_jq || exit 2
mkdir -p "${EVIDENCE}"
cd "${REPO_ROOT}"

STARTED="$(epoch_s)"
FAILURES_FILE="${EVIDENCE}/failures.tsv"
: > "${FAILURES_FILE}"
HARD_FAILS=()

section "GATE 1/4 · tests — suites green (rust subset + desktop)"

# run_logged <label> <logfile> -- <argv...>
# Streams to console AND to evidence. Returns the command's status, not tee's.
run_logged() {
  local label="$1" logfile="$2"; shift 2
  [[ "${1:-}" == "--" ]] && shift
  step "${label}"
  preview "$@"
  is_dry && { note "output -> ${logfile}"; return 0; }
  # A real pipeline, not process substitution: a pipeline guarantees tee has
  # finished writing before we return, so the failure parser below never reads
  # a half-flushed log.
  set +e
  "$@" 2>&1 | tee "${logfile}"
  local rc=${PIPESTATUS[0]}
  set -e
  return "${rc}"
}

if ! run_logged "gate harness schema contract" \
    "${EVIDENCE}/harness-schema-contract.log" -- \
    ./scripts/selfhost/gates/test-harness-schema-contract.sh; then
  HARD_FAILS+=("gate harness schema contract failed (gate mechanism error — not waivable)")
fi

if ! run_logged "gate relay-key contract" \
    "${EVIDENCE}/harness-relay-key-contract.log" -- \
    ./scripts/selfhost/gates/test-harness-relay-key-contract.sh; then
  HARD_FAILS+=("gate relay-key contract failed (gate mechanism error — not waivable)")
fi

# ---- Rust ------------------------------------------------------------------

if [[ "${SKIP_RUST}" != "1" ]]; then
  ensure_cargo || exit 2
  export CARGO_TERM_COLOR=never

  if ! run_logged "cargo check --all-targets (buzz-core, buzz-sdk, buzz-cli, buzz-acp)" \
      "${EVIDENCE}/rust-check.log" -- cargo check "${RUST_PKGS[@]}" --all-targets; then
    HARD_FAILS+=("cargo check --all-targets failed (compile error — not waivable)")
  fi

  # --no-fail-fast so ONE red test cannot hide the rest. A gate that stops at
  # the first failure makes the waiver file impossible to keep honest.
  if ! run_logged "cargo test (same four packages, --no-fail-fast)" \
      "${EVIDENCE}/rust-test.log" -- cargo test "${RUST_PKGS[@]}" --no-fail-fast; then
    if ! is_dry; then
      cargo_failures "${EVIDENCE}/rust-test.log" >> "${FAILURES_FILE}"
      if [[ ! -s "${FAILURES_FILE}" ]]; then
        HARD_FAILS+=("cargo test failed but no test names were parsed — build/link error, see rust-test.log")
      fi
    fi
  fi

  # The relay's test targets cannot build here (openssl). Type-check the library
  # so a non-compiling candidate relay is still caught by gate 1 rather than
  # surfacing later as a mystery in gate 3.
  if ! run_logged "cargo check -p buzz-relay --lib  (openssl gap: relay TEST targets cannot build on this host)" \
      "${EVIDENCE}/rust-relay-check.log" -- cargo check -p buzz-relay --lib; then
    HARD_FAILS+=("cargo check -p buzz-relay --lib failed (compile error — not waivable)")
  fi
else
  note "rust phase skipped (--skip-rust)"
fi

# ---- Desktop ---------------------------------------------------------------

if [[ "${SKIP_DESKTOP}" != "1" ]]; then
  ensure_node || exit 2

  # cd rather than a ( subshell ): a subshell would discard the step counter's
  # increments, so the printed plan would restart its numbering mid-gate.
  cd "${REPO_ROOT}/desktop"

  if ! run_logged "pnpm typecheck (desktop)" \
        "${EVIDENCE}/desktop-typecheck.log" -- pnpm typecheck; then
    HARD_FAILS+=("desktop pnpm typecheck failed (type error — not waivable)")
  fi

  if [[ "${SKIP_DESKTOP_TESTS}" != "1" ]]; then
    if ! run_logged "pnpm test (desktop)" \
          "${EVIDENCE}/desktop-test.log" -- pnpm test; then
      if ! is_dry; then
        node_failures "${EVIDENCE}/desktop-test.log" >> "${FAILURES_FILE}"
      fi
    fi
  else
    note "desktop test suite skipped (--skip-desktop-tests); typecheck still ran"
  fi
  cd "${REPO_ROOT}"
else
  note "desktop phase skipped (--skip-desktop)"
fi

# ---- Verdict ---------------------------------------------------------------

if is_dry; then
  record_result "${EVIDENCE}" tests dry-run "${STARTED}" \
    '{"note":"planned only; no evidence of correctness"}'
  print_result_line tests dry-run 0 "${EVIDENCE}"
  exit 0
fi

waivers_load "${WAIVER_FILE}"
set +e
waivers_classify "${FAILURES_FILE}"
CLEAN=$?
set -e
waivers_report "${WAIVER_FILE}"

RESULT=pass
if [[ ${#HARD_FAILS[@]} -gt 0 ]]; then
  RESULT=fail
  banner "${RED}" "NON-WAIVABLE FAILURES" "" "${HARD_FAILS[@]}"
elif [[ "${CLEAN}" -ne 0 ]]; then
  RESULT=fail
fi

DETAILS="$(jq -n \
  --argjson waivers "$(waivers_json "${WAIVER_FILE}")" \
  --argjson hard "$(printf '%s\n' "${HARD_FAILS[@]:-}" | jq -R . | jq -s 'map(select(length>0))')" \
  --arg rust_pkgs "buzz-core buzz-sdk buzz-cli buzz-acp" \
  --argjson desktop_tests_run "$([[ "${SKIP_DESKTOP}" != "1" && "${SKIP_DESKTOP_TESTS}" != "1" ]] && echo true || echo false)" \
  '{rust_packages:$rust_pkgs, desktop_tests_run:$desktop_tests_run,
    non_waivable_failures:$hard, waivers:$waivers,
    known_gap:"buzz-relay test targets not built: no openssl dev headers on this host; only `cargo check -p buzz-relay --lib` ran"}')"

record_result "${EVIDENCE}" tests "${RESULT}" "${STARTED}" "${DETAILS}"
DURATION=$(( $(epoch_s) - STARTED ))
print_result_line tests "${RESULT}" "${DURATION}" "${EVIDENCE}"
[[ "${RESULT}" == "pass" ]]
