#!/usr/bin/env bash
# =============================================================================
# run-gates.sh — staging promotion gate runner for the self-hosted Buzz loop.
# =============================================================================
# Takes candidate artifacts, stands them up against the ISOLATED harness stack,
# runs the promotion gates in order of cheapness, and stamps a hash-bound
# verdict at gates/promote-stamp.json. That stamp is the deployer's input.
#
#   ./scripts/selfhost/gates/run-gates.sh <subcommand> [options]
#
# SUBCOMMANDS
#   all           every gate in cheapness order, then stamp
#   tests         gate 1 — cargo (buzz-core/sdk/cli/acp) + desktop typecheck/test
#   conformance   gate 2 — TLA+ trace replay checker
#   skew          gate 3 — (relay@candidate × acp@deployed), (relay@deployed × acp@candidate)
#   soak          gate 4 — candidate × candidate, looped
#   stamp         (re)stamp from an existing run directory
#   lock          print the candidate hash lock and exit
#   teardown      force-teardown the gates compose project
#
# OPTIONS
#   --dry-run              print the plan, run nothing  [DEFAULT]
#   --execute              actually run
#   --profile <p>          cargo profile               [ci]
#   --project-name <n>     compose project             [buzz-gates]
#   --evidence-root <dir>  evidence tree               [/tmp/buzz-gates]
#   --run-id <id>          run identifier              [<UTC timestamp>-<commit>]
#   --soak-duration <s>    soak seconds                [300]
#   --waivers <file>       waiver file                 [gates/waivers.txt]
#   --skip-desktop-tests   run desktop typecheck but not the ~100s test suite
#   --build / --no-build   build candidate artifacts before taking the hash
#                          lock                        [--build when executing]
#   --keep-going           run every gate even after one fails
#   --no-teardown          leave the compose stack up (debugging; says so loudly)
#   --json                 print the machine-readable summary at the end
#
# DEFAULTING TO --dry-run IS DELIBERATE. Gates 3 and 4 start containers and
# build binaries; a runner that did that on a bare invocation would be a
# footgun on a box shared with live stacks.
#
# SAFETY
#   * Compose project is `buzz-gates`, never `buzz-harness` (a sibling
#     worktree's stack, routinely up for days) and never `buzz-prod`. The
#     forbidden list is enforced in lib/harness.sh:harness_guard.
#   * Ports are shifted off the harness block so both can coexist.
#   * Teardown is a trap, and it is VERIFIED (harness_assert_torn_down) rather
#     than assumed.
#   * Nothing here reads or writes /opt/buzz/keys, /etc, systemd, or the
#     buzz-prod stack. The deployed artifacts (/opt/buzz/bin/buzz-acp and the
#     buzz-local image) are read-only inputs — hashed, executed in the harness,
#     never modified.
# =============================================================================
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="gates"
# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"
# shellcheck source=lib/candidate.sh
source "${GATES_DIR}/lib/candidate.sh"

SUBCOMMAND=""
export GATES_EXECUTE="${GATES_EXECUTE:-0}"
export GATES_PROFILE="${GATES_PROFILE:-ci}"
export GATES_PROJECT="${GATES_PROJECT:-buzz-gates}"
export GATES_SOAK_DURATION="${GATES_SOAK_DURATION:-300}"
export GATES_WAIVER_FILE="${GATES_WAIVER_FILE:-${GATES_DIR}/waivers.txt}"
export GATES_SKIP_DESKTOP_TESTS="${GATES_SKIP_DESKTOP_TESTS:-0}"
export GATES_NO_TEARDOWN="${GATES_NO_TEARDOWN:-0}"
EVIDENCE_ROOT="${GATES_EVIDENCE_ROOT:-/tmp/buzz-gates}"
RUN_ID=""
KEEP_GOING=0
PRINT_JSON=0
# Build candidate artifacts before taking the hash lock. On by default when
# executing; --no-build is for gating binaries someone else produced.
DO_BUILD=1

usage() { sed -n '2,55p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    all|tests|conformance|skew|soak|stamp|lock|teardown) SUBCOMMAND="$1"; shift ;;
    --execute)            GATES_EXECUTE=1; shift ;;
    --dry-run)            GATES_EXECUTE=0; shift ;;
    --profile)            GATES_PROFILE="$2"; shift 2 ;;
    --project-name)       GATES_PROJECT="$2"; shift 2 ;;
    --evidence-root)      EVIDENCE_ROOT="$2"; shift 2 ;;
    --run-id)             RUN_ID="$2"; shift 2 ;;
    --soak-duration)      GATES_SOAK_DURATION="$2"; shift 2 ;;
    --waivers)            GATES_WAIVER_FILE="$2"; shift 2 ;;
    --skip-desktop-tests) GATES_SKIP_DESKTOP_TESTS=1; shift ;;
    --keep-going)         KEEP_GOING=1; shift ;;
    --build)              DO_BUILD=1; shift ;;
    --no-build)           DO_BUILD=0; shift ;;
    --no-teardown)        GATES_NO_TEARDOWN=1; shift ;;
    --json)               PRINT_JSON=1; shift ;;
    -h|--help)            usage; exit 0 ;;
    *) err "unknown argument: $1"; echo; usage >&2; exit 2 ;;
  esac
done

[[ -n "${SUBCOMMAND}" ]] || { err "a subcommand is required"; echo; usage >&2; exit 2; }
require_jq || exit 2
cd "${REPO_ROOT}"

if [[ -z "${RUN_ID}" ]]; then
  RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$(git -C "${REPO_ROOT}" rev-parse --short HEAD 2>/dev/null || echo nogit)"
fi
RUN_DIR="${EVIDENCE_ROOT}/${RUN_ID}"
export GATES_RUN_DIR="${RUN_DIR}"

# ---- teardown / lock subcommands (no run dir needed) ------------------------

if [[ "${SUBCOMMAND}" == "teardown" ]]; then
  # shellcheck source=lib/harness.sh
  source "${GATES_DIR}/lib/harness.sh"
  harness_guard || exit 2
  section "TEARDOWN · compose project ${GATES_PROJECT}"
  runx "Down (with volumes and orphans)" \
    -- docker compose -p "${GATES_PROJECT}" \
       -f "${REPO_ROOT}/docker-compose.harness.yml" \
       -f "${REPO_ROOT}/scripts/selfhost/gates/docker-compose.gates.yml" \
       --profile deployed-relay down -v --remove-orphans || true
  is_dry || harness_assert_torn_down
  exit $?
fi

if [[ "${SUBCOMMAND}" == "lock" ]]; then
  candidate_lock_json "${REPO_ROOT}" "${GATES_PROFILE}" | jq .
  exit 0
fi

# ---- banner -----------------------------------------------------------------

section "BUZZ STAGING GATES · run ${RUN_ID}"
if is_dry; then
  warn "DRY RUN (default). Nothing will be built, started, or torn down."
  warn "Re-run with --execute to actually gate this candidate."
else
  log "EXECUTING. compose project=${GATES_PROJECT}, cargo profile=${GATES_PROFILE}"
fi
log "repo        ${REPO_ROOT}"
log "commit      $(git -C "${REPO_ROOT}" rev-parse HEAD 2>/dev/null || echo unknown) ($(git -C "${REPO_ROOT}" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?'))"
log "evidence    ${RUN_DIR}"
log "waivers     ${GATES_WAIVER_FILE}"

mkdir -p "${RUN_DIR}"

# ---- candidate lock ---------------------------------------------------------
# Taken BEFORE any gate runs. stamp.sh re-hashes and refuses on drift.

if [[ "${SUBCOMMAND}" != "stamp" ]]; then
  # Build BEFORE locking. The lock has to describe finished candidate bytes: if
  # it is taken against a stale (or absent) binary that gate 1 then rebuilds as
  # a side effect of `cargo test`, stamp.sh sees the rebuild as drift and
  # refuses a run that was in fact perfectly valid. Observed exactly that on the
  # first real run of this pipeline. Building first makes any later hash change
  # genuine drift — someone else's rebuild — which is what the refusal is for.
  if [[ "${DO_BUILD}" == "1" ]] && ! is_dry; then
    ensure_cargo || exit 2
    step "Build candidate artifacts before locking (profile=${GATES_PROFILE})"
    preview cargo build --profile "${GATES_PROFILE}" -p buzz-relay -p buzz-acp -p buzz-cli
    CARGO_TERM_COLOR=never cargo build --profile "${GATES_PROFILE}" \
      -p buzz-relay -p buzz-acp -p buzz-cli 2>&1 | tail -5 \
      || { err "candidate build failed — nothing to gate"; exit 1; }

    # Freeze the candidate outside target/. cargo rewrites target/ binaries
    # whenever a later invocation selects a different package set (feature
    # unification), so gating target/ directly means gating a moving target.
    # See lib/candidate.sh § staged artifacts.
    step "Stage candidate artifacts (immutable copy — cargo cannot rewrite these)"
    candidate_stage "${REPO_ROOT}" "${GATES_PROFILE}" "${RUN_DIR}/artifacts" \
      || { err "failed to stage candidate artifacts"; exit 1; }
    export GATES_ARTIFACT_DIR="${RUN_DIR}/artifacts"
    log "staged -> ${GATES_ARTIFACT_DIR}"
  elif [[ "${DO_BUILD}" != "1" ]]; then
    if [[ -d "${RUN_DIR}/artifacts" ]]; then
      export GATES_ARTIFACT_DIR="${RUN_DIR}/artifacts"
      note "--no-build: reusing already-staged artifacts in ${GATES_ARTIFACT_DIR}"
    else
      note "--no-build and nothing staged: locking target/ directly. A gate that"
      note "rebuilds with a different package selection WILL trip the hash binding."
    fi
  fi

  step "Take the candidate hash lock (source commit + artifact sha256s)"
  candidate_lock_json "${REPO_ROOT}" "${GATES_PROFILE}" > "${RUN_DIR}/candidate-lock.json"
  jq -r '.artifacts[] | "      \(.role)\t\(if .present then .sha256[0:16] else "ABSENT" end)\t\(.path)"' \
    "${RUN_DIR}/candidate-lock.json"
  if [[ "$(jq -r '.worktree_dirty' "${RUN_DIR}/candidate-lock.json")" == "true" ]]; then
    warn "worktree is DIRTY — the stamp will record this. Shared worktree: expected here, not in CI."
  fi
fi

# ---- gate dispatch ----------------------------------------------------------

declare -a RAN=()
OVERALL=0

run_gate() {
  local name="$1" script="$2"; shift 2
  local ev="${RUN_DIR}/${name}"
  mkdir -p "${ev}"
  RAN+=("${name}")
  local flag=--dry-run
  is_dry || flag=--execute
  set +e
  GATES_EVIDENCE="${ev}" "${GATES_DIR}/${script}" "${flag}" --evidence "${ev}" "$@"
  local rc=$?
  set -e
  [[ ${rc} -eq 0 ]] || OVERALL=1
  return ${rc}
}

gate_tests() {
  local extra=()
  [[ "${GATES_SKIP_DESKTOP_TESTS}" == "1" ]] && extra+=(--skip-desktop-tests)
  # ${arr[@]+"${arr[@]}"} — not "${arr[@]:-}", which under set -u expands an
  # EMPTY array to one empty-string argument and makes the gate reject "" as an
  # unknown option.
  run_gate tests gate-tests.sh --waivers "${GATES_WAIVER_FILE}" ${extra[@]+"${extra[@]}"}
}
gate_conformance() { run_gate conformance gate-conformance.sh; }
gate_skew()        { run_gate skew gate-skew.sh --profile "${GATES_PROFILE}" --project-name "${GATES_PROJECT}"; }
gate_soak()        { run_gate soak gate-soak.sh --profile "${GATES_PROFILE}" --project-name "${GATES_PROJECT}" --soak-duration "${GATES_SOAK_DURATION}"; }

case "${SUBCOMMAND}" in
  tests)       gate_tests       || true ;;
  conformance) gate_conformance || true ;;
  skew)        gate_skew        || true ;;
  soak)        gate_soak        || true ;;
  stamp)       ;;
  all)
    # Cheapness order: a compile error should cost seconds, not a container
    # bring-up and a five-minute soak.
    for g in tests conformance skew soak; do
      if ! "gate_${g}"; then
        if [[ "${KEEP_GOING}" != "1" ]]; then
          err "gate '${g}' failed — stopping (pass --keep-going to run the rest anyway)"
          break
        fi
        warn "gate '${g}' failed — continuing (--keep-going)"
      fi
    done
    ;;
esac

# ---- stamp ------------------------------------------------------------------

section "SUMMARY · run ${RUN_ID}"
for g in "${RAN[@]:-}"; do
  [[ -n "${g}" ]] || continue
  rf="${RUN_DIR}/${g}/result.json"
  if [[ -r "${rf}" ]]; then
    print_result_line "$(jq -r .name "${rf}")" "$(jq -r .result "${rf}")" \
      "$(jq -r .duration_s "${rf}")" "$(jq -r .evidence "${rf}")"
  else
    print_result_line "${g}" missing 0 "${RUN_DIR}/${g}"
    OVERALL=1
  fi
done

EXPECTED="${RAN[*]:-}"
[[ "${SUBCOMMAND}" == "all" || "${SUBCOMMAND}" == "stamp" ]] && EXPECTED="tests conformance skew soak"

set +e
GATES_PROFILE="${GATES_PROFILE}" "${GATES_DIR}/stamp.sh" \
  --run-dir "${RUN_DIR}" --profile "${GATES_PROFILE}" --expected "${EXPECTED}"
STAMP_RC=$?
set -e

if [[ "${PRINT_JSON}" == "1" ]]; then
  jq -c '{run_id, verdict, verdict_reason, gates: [.gates[] | {name, result}]}' \
    "${GATES_DIR}/promote-stamp.json"
fi

log "Deployer input: ${GATES_DIR}/promote-stamp.json (see gates/README.md § Handoff to the deployer)"
[[ ${OVERALL} -eq 0 && ${STAMP_RC} -eq 0 ]]
