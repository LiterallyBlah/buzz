#!/usr/bin/env bash
# =============================================================================
# stamp.sh — produce gates/promote-stamp.json.
# =============================================================================
# The stamp is the ONLY output of this pipeline that another system consumes.
# It is deliberately boring: it collects the per-gate result.json files, re-hashes
# the candidate artifacts, and writes a verdict. It runs NO tests itself — a
# stamper that could also produce evidence could produce evidence for a stamp
# nobody ran.
#
# HASH BINDING
#   run-gates.sh writes candidate-lock.json BEFORE the first gate. stamp.sh
#   recomputes the same description and compares. If the bytes moved, the
#   verdict is `refused` and the drift is spelled out — because a green gate
#   run about binary X says nothing about binary Y, and shipping Y on X's
#   evidence is precisely the failure this whole pipeline exists to prevent.
#
# VERDICTS
#   promotable                — every gate passed, no waivers, artifacts bound
#   promotable_with_waivers   — as above BUT waivers were applied. Still bound,
#                               still all-gates-green, but a human decided some
#                               red tests do not block. The deployer is expected
#                               to have a policy for this; it must not treat the
#                               string as equal to `promotable`.
#   blocked                   — at least one gate failed
#   refused                   — artifacts or source moved mid-run; the evidence
#                               does not describe the current bytes. NEVER deploy
#                               a refused stamp, regardless of gate results.
#   incomplete                — a gate never produced a result.json (crash, kill)
# =============================================================================
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="stamp"
# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"
# shellcheck source=lib/candidate.sh
source "${GATES_DIR}/lib/candidate.sh"

STAMP_SCHEMA="buzz.staging.promote-stamp/v1"
RUN_DIR="${GATES_RUN_DIR:-}"
OUT="${GATES_STAMP_OUT:-${GATES_DIR}/promote-stamp.json}"
PROFILE="${GATES_PROFILE:-ci}"
EXPECTED_GATES="${GATES_EXPECTED:-tests conformance skew soak}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run-dir)  RUN_DIR="$2"; shift 2 ;;
    --out)      OUT="$2"; shift 2 ;;
    --profile)  PROFILE="$2"; shift 2 ;;
    --expected) EXPECTED_GATES="$2"; shift 2 ;;
    --execute|--dry-run) shift ;;   # accepted for symmetry; stamping is always real
    *) err "unknown option: $1"; exit 2 ;;
  esac
done

require_jq || exit 2
[[ -n "${RUN_DIR}" ]] || { err "--run-dir is required"; exit 2; }
[[ -d "${RUN_DIR}" ]] || { err "run dir does not exist: ${RUN_DIR}"; exit 2; }

# Re-hash the same bytes the lock described. run-gates.sh exports this, but
# stamp.sh is also runnable standalone against an old run dir — in which case
# the staged artifacts are still sitting there and are still the right thing to
# hash. Falling back to target/ would compare a frozen candidate against
# whatever cargo has since rewritten, and refuse every run.
if [[ -z "${GATES_ARTIFACT_DIR:-}" && -d "${RUN_DIR}/artifacts" ]]; then
  export GATES_ARTIFACT_DIR="${RUN_DIR}/artifacts"
fi

LOCK="${RUN_DIR}/candidate-lock.json"
VERIFY="${RUN_DIR}/candidate-verify.json"
[[ -r "${LOCK}" ]] || { err "no candidate-lock.json in ${RUN_DIR} — cannot bind a stamp to unknown bytes"; exit 2; }

section "STAMP · re-verifying candidate and collecting gate results"

# ---- re-hash ----------------------------------------------------------------

candidate_lock_json "${REPO_ROOT}" "${PROFILE}" > "${VERIFY}"
DRIFT="$(candidate_verify "${LOCK}" "${VERIFY}")"
BOUND="$(echo "${DRIFT}" | jq -r '.bound')"

if [[ "${BOUND}" == "true" ]]; then
  ok "candidate is bound: artifacts and HEAD unchanged since the run started"
else
  err "candidate DRIFTED during the run:"
  echo "${DRIFT}" | jq . >&2
fi

if [[ "$(echo "${DRIFT}" | jq -r '.tree_digest_changed')" == "true" ]]; then
  warn "build-input tree digest changed during the run (advisory)."
  warn "Source moved but the tested artifacts did not — a rebuild would produce different bytes."
fi

# ---- collect gate results ---------------------------------------------------

GATES_JSON='[]'
MISSING=()
for g in ${EXPECTED_GATES}; do
  rf="${RUN_DIR}/${g}/result.json"
  if [[ -r "${rf}" ]]; then
    GATES_JSON="$(jq -n --argjson acc "${GATES_JSON}" --slurpfile r "${rf}" '$acc + [$r[0]]')"
  else
    MISSING+=("${g}")
    GATES_JSON="$(jq -n --argjson acc "${GATES_JSON}" --arg n "${g}" \
      '$acc + [{name:$n, result:"missing", duration_s:0, evidence:null,
                details:{error:"gate produced no result.json"}}]')"
  fi
done

FAILED="$(echo "${GATES_JSON}"  | jq '[.[] | select(.result == "fail")] | length')"
DRYRUN="$(echo "${GATES_JSON}"  | jq '[.[] | select(.result == "dry-run")] | length')"
WAIVED="$(echo "${GATES_JSON}"  | jq '[.[] | .details.waivers.applied // [] | length] | add // 0')"

# ---- verdict ----------------------------------------------------------------

VERDICT=promotable
REASON="all gates passed; candidate artifacts unchanged since the run began"

if [[ "${BOUND}" != "true" ]]; then
  VERDICT=refused
  REASON="candidate artifacts or HEAD changed mid-run; the gate evidence does not describe these bytes"
elif [[ ${#MISSING[@]} -gt 0 ]]; then
  VERDICT=incomplete
  REASON="gate(s) produced no result: ${MISSING[*]}"
elif [[ "${FAILED}" -gt 0 ]]; then
  VERDICT=blocked
  REASON="${FAILED} gate(s) failed"
elif [[ "${DRYRUN}" -gt 0 ]]; then
  VERDICT=incomplete
  REASON="${DRYRUN} gate(s) were dry-run only — a plan is not evidence"
elif [[ "${WAIVED}" -gt 0 ]]; then
  VERDICT=promotable_with_waivers
  REASON="${WAIVED} waived test failure(s) in force — see gates[].details.waivers and waivers.txt"
fi

# ---- write ------------------------------------------------------------------

jq -n \
  --arg schema "${STAMP_SCHEMA}" \
  --arg run_id "$(basename "${RUN_DIR}")" \
  --arg run_dir "${RUN_DIR}" \
  --slurpfile candidate "${VERIFY}" \
  --argjson baseline "$(baseline_json)" \
  --argjson binding "${DRIFT}" \
  --argjson gates "${GATES_JSON}" \
  --arg stamped_at "$(iso_now)" \
  --arg verdict "${VERDICT}" \
  --arg verdict_reason "${REASON}" \
  --arg host "$(hostname 2>/dev/null || echo unknown)" \
  --arg runner "scripts/selfhost/gates/run-gates.sh" \
  '{schema:$schema,
    run_id:$run_id,
    candidate:$candidate[0],
    baseline:$baseline,
    binding:$binding,
    gates:$gates,
    stamped_at:$stamped_at,
    verdict:$verdict,
    verdict_reason:$verdict_reason,
    promoted_by:{runner:$runner, host:$host, run_dir:$run_dir}}' \
  > "${OUT}"

echo
case "${VERDICT}" in
  promotable)              ok    "VERDICT: promotable — ${REASON}" ;;
  promotable_with_waivers) banner "${YELLOW}" "VERDICT: promotable_with_waivers" "" "${REASON}" \
                             "This is NOT the same as 'promotable'. The deployer must" \
                             "decide whether it accepts a stamp carrying waivers." ;;
  refused)                 banner "${RED}" "VERDICT: refused — DO NOT DEPLOY" "" "${REASON}" ;;
  blocked)                 banner "${RED}" "VERDICT: blocked" "" "${REASON}" ;;
  *)                       warn  "VERDICT: ${VERDICT} — ${REASON}" ;;
esac
log "stamp written: ${OUT}"

[[ "${VERDICT}" == "promotable" || "${VERDICT}" == "promotable_with_waivers" ]]
