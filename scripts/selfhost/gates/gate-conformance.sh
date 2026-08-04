#!/usr/bin/env bash
# =============================================================================
# gate-conformance.sh — GATE 2: TLA+ trace conformance.
# =============================================================================
# The north star from crates/buzz-conformance/src/lib.rs:6-7 —
#   "don't ask 'did the model pass'; ask 'did the running code emit a trace
#    the model accepts.'"
#
# PHASE A — checker integrity  [REAL, runs today]
#   `cargo test -p buzz-conformance --all-targets`. This is not decorative:
#   the crate ships adversarial replay fixtures (tests/fixtures/*.jsonl) that
#   assert check_trace FAILS on a host/channel fence skip, on a foreign-row
#   leak, and on a coverage breach, plus proptests over the checker. Phase A
#   proves the judge still bites. If the checker were silently reduced to
#   `Ok(())`, phase A is what catches it.
#   buzz-conformance depends on no production buzz crate (see its Cargo.toml
#   "Independence rule"), so it builds and tests cleanly on this host — the
#   openssl gap that blocks buzz-relay's test targets does not touch it.
#
# PHASE B — live trace replay  [BLOCKED — see below, and gates/README.md]
#   Intended: run the candidate relay against the isolated harness with trace
#   emission ON, drive a scripted workload, then replay the emitted JSONL
#   through check_trace. This gate implements everything up to the point where
#   the missing wiring stops it, then reports BLOCKED with the exact
#   file:line — it does NOT pretend phase A covered phase B.
#
#   Two concrete blockers, both probed at runtime by `probe_emission_wiring`
#   so this gate starts working by itself the day either is fixed:
#
#   B1. No runtime tracer binding. AppState hardcodes the no-op tracer at
#       construction and nothing ever replaces it — `main.rs` never touches
#       `state.tracer`. The emitters themselves ARE wired (ingest.rs and
#       req.rs both call into the tracer), so the only missing piece is a
#       switch. Needed: honour an env var (proposed name below) and bind
#       `crate::conformance::JsonlTracer::create(path)` instead of NoopTracer.
#         proposed contract: BUZZ_CONFORMANCE_TRACE_PATH=<file>
#
#   B2. No replay entrypoint. `buzz_conformance::checker::check_trace` is a
#       library API and the crate declares no [[bin]] and no [[example]]
#       (crates/buzz-conformance/Cargo.toml), so a shell gate has nothing to
#       invoke on a captured .jsonl. Needed: a small bin/example that reads
#       JSONL on stdin, builds a Scenario, calls check_trace, exits non-zero
#       on Err.
#
#   Both fixes live OUTSIDE scripts/selfhost/gates/, which this runner does not
#   own — hence: reported, not silently patched.
# =============================================================================
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="gate:conformance"
# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"

EVIDENCE="${GATES_EVIDENCE:-/tmp/buzz-gates/adhoc/conformance}"
TRACE_ENV_VAR="BUZZ_CONFORMANCE_TRACE_PATH"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --execute)  GATES_EXECUTE=1; shift ;;
    --dry-run)  GATES_EXECUTE=0; shift ;;
    --evidence) EVIDENCE="$2"; shift 2 ;;
    *) err "unknown option: $1"; exit 2 ;;
  esac
done

require_jq || exit 2
mkdir -p "${EVIDENCE}"
cd "${REPO_ROOT}"
STARTED="$(epoch_s)"

section "GATE 2/4 · conformance — TLA+ trace replay (MultiTenantRelay.tla)"

# ---- Blocker probe ----------------------------------------------------------
# Located dynamically so the TODO in this gate's output cites a live file:line
# instead of a number that rots the moment someone edits state.rs.

probe_emission_wiring() {
  BLOCKER_B1=""; BLOCKER_B2=""

  local noop_line runtime_binding
  noop_line="$(grep -n 'tracer: Arc::new(crate::conformance::NoopTracer)' \
                 crates/buzz-relay/src/state.rs 2>/dev/null | head -1 | cut -d: -f1)"
  # Has anyone landed a runtime switch? Either the env var appears in relay
  # source, or something outside tracers.rs constructs a JsonlTracer.
  runtime_binding="$(grep -rl "${TRACE_ENV_VAR}\|JsonlTracer::create" \
                       crates/buzz-relay/src --include='*.rs' 2>/dev/null \
                     | grep -v 'conformance/tracers.rs' || true)"

  if [[ -z "${runtime_binding}" ]]; then
    BLOCKER_B1="crates/buzz-relay/src/state.rs:${noop_line:-?} — AppState::new hardcodes NoopTracer and nothing rebinds it; no ${TRACE_ENV_VAR} handling anywhere in crates/buzz-relay/src. Relay cannot be asked to emit a trace from outside the process."
  fi

  if ! grep -qE '^\[\[(bin|example)\]\]' crates/buzz-conformance/Cargo.toml 2>/dev/null \
     && [[ ! -d crates/buzz-conformance/src/bin ]] \
     && [[ ! -d crates/buzz-conformance/examples ]]; then
    BLOCKER_B2="crates/buzz-conformance/Cargo.toml — library-only crate: no [[bin]], no [[example]], no src/bin/, no examples/. check_trace (crates/buzz-conformance/src/checker.rs:74) has no shell-invokable entrypoint to replay a captured .jsonl."
  fi

  [[ -z "${BLOCKER_B1}" && -z "${BLOCKER_B2}" ]]
}

# ---- Phase A ----------------------------------------------------------------

PHASE_A=skipped
ensure_cargo || exit 2
export CARGO_TERM_COLOR=never

step "PHASE A — checker integrity: cargo test -p buzz-conformance --all-targets"
preview cargo test -p buzz-conformance --all-targets
note "proves the replay checker still FAILS on the adversarial fixtures:"
note "  tests/fixtures/bad_host_channel_mismatch.jsonl -> IllegalTransition"
note "  tests/fixtures/bad_foreign_row_leak.jsonl      -> NonInterference"
note "  tests/fixtures/bad_coverage_breach.jsonl       -> CoverageBreach"

if ! is_dry; then
  set +e
  cargo test -p buzz-conformance --all-targets > >(tee "${EVIDENCE}/conformance-test.log") 2>&1
  rc=${PIPESTATUS[0]}
  set -e
  if [[ ${rc} -eq 0 ]]; then PHASE_A=pass; ok "Phase A green"; else PHASE_A=fail; err "Phase A FAILED — the trace checker itself is broken"; fi
fi

# ---- Phase B ----------------------------------------------------------------

PHASE_B=blocked
step "PHASE B — live trace replay against the candidate relay"
note "would: harness up -> relay with ${TRACE_ENV_VAR}=<trace.jsonl> -> scripted workload -> replay through check_trace"

if ! is_dry; then
  if probe_emission_wiring; then
    # Someone landed the wiring. Refuse to claim a pass we have not actually
    # implemented the driver for — fail loudly so it gets finished, rather than
    # quietly reporting green.
    PHASE_B=fail
    banner "${YELLOW}" \
      "CONFORMANCE PHASE B IS NOW UNBLOCKED" \
      "" \
      "Relay trace emission and a replay entrypoint both exist now." \
      "Finish the live-capture driver in gate-conformance.sh (this file)" \
      "and delete this branch."
  else
    {
      echo "PHASE B: BLOCKED"
      echo
      [[ -n "${BLOCKER_B1}" ]] && { echo "B1 ${BLOCKER_B1}"; echo; }
      [[ -n "${BLOCKER_B2}" ]] && { echo "B2 ${BLOCKER_B2}"; echo; }
      echo "Emitters that ARE already wired (so only the switch is missing):"
      grep -n 'state.tracer' crates/buzz-relay/src/handlers/ingest.rs crates/buzz-relay/src/handlers/req.rs 2>/dev/null | head -10
    } | tee "${EVIDENCE}/phase-b-blockers.txt"
    warn "Phase B BLOCKED — recorded in ${EVIDENCE}/phase-b-blockers.txt"
    [[ -n "${BLOCKER_B1}" ]] && err "  TODO B1: ${BLOCKER_B1}"
    [[ -n "${BLOCKER_B2}" ]] && err "  TODO B2: ${BLOCKER_B2}"
  fi
fi

# ---- Verdict ----------------------------------------------------------------
# Phase A passing while phase B is blocked is reported as `pass` for the
# pipeline, but details.phase_b carries `blocked` and the README states plainly
# that gate 2 currently proves the CHECKER, not the RELAY. The stamp carries
# that distinction so a reader is never misled by the one-word result.

if is_dry; then
  record_result "${EVIDENCE}" conformance dry-run "${STARTED}" \
    '{"note":"planned only","phase_a":"cargo test -p buzz-conformance","phase_b":"blocked (see gate header)"}'
  print_result_line conformance dry-run 0 "${EVIDENCE}"
  exit 0
fi

RESULT=pass
[[ "${PHASE_A}" == "pass" ]] || RESULT=fail
[[ "${PHASE_B}" == "fail" ]] && RESULT=fail

DETAILS="$(jq -n \
  --arg phase_a "${PHASE_A}" \
  --arg phase_b "${PHASE_B}" \
  --arg b1 "${BLOCKER_B1:-}" \
  --arg b2 "${BLOCKER_B2:-}" \
  '{phase_a:{name:"checker integrity (cargo test -p buzz-conformance)", result:$phase_a},
    phase_b:{name:"live trace replay against candidate relay", result:$phase_b,
             blockers:[$b1,$b2]|map(select(length>0))},
    proves:"the independent replay checker still rejects illegal transitions, foreign-row leaks and coverage breaches",
    does_not_prove:"that THIS candidate relay emits a spec-conformant trace — no runtime trace emission exists yet"}')"

record_result "${EVIDENCE}" conformance "${RESULT}" "${STARTED}" "${DETAILS}"
DURATION=$(( $(epoch_s) - STARTED ))
print_result_line conformance "${RESULT}" "${DURATION}" "${EVIDENCE}"
[[ "${RESULT}" == "pass" ]]
