#!/usr/bin/env bash
# =============================================================================
# gate-soak.sh — GATE 4: sustained-load soak.
# =============================================================================
# WHAT THIS GATE PROVES
#   The candidate pairing (relay@candidate × acp@candidate) survives the same
#   synthetic workload repeated for a wall-clock duration without degrading:
#     * every iteration completes (announce repo -> issue -> enrol -> reply)
#     * no ERROR lines accumulate in relay or harness logs
#     * every `dropped=` / `dropped_total=` counter stays 0 — i.e. no publisher
#       lagged (crates/buzz-acp/src/lib.rs:887,:985) and no gated frames were
#       shed (relay.rs:1611,:1645)
#     * RSS of both processes is sampled per iteration so an obvious leak shows
#       up as monotonic growth rather than as a mystery in production a week later
#
# DURATION — HONESTLY DEFAULTED, NOT HONESTLY SUFFICIENT
#   Default is 300s (5 minutes). That is a CI-of-one smoke soak: it catches
#   fast leaks, immediate handle exhaustion, and reconnect storms. It does NOT
#   catch slow leaks, daily-cycle effects, log rotation, disk fill, or
#   connection churn over hours. A real pre-release soak runs for hours against
#   production-shaped traffic:
#       ./run-gates.sh soak --execute --soak-duration 14400
#   The stamp records the duration actually used, so nobody can mistake a
#   5-minute run for an overnight one. Read `gates[].details.duration_s` before
#   trusting a soak result.
#
# WHAT IT DELIBERATELY DOES NOT PROVE
#   * Nothing about concurrency: this is ONE agent taking ONE turn at a time.
#     It is a duration test, not a load test. Throughput/contention belongs in
#     benchmarks/ and perf/, not here.
#   * Nothing about multi-tenant behaviour under load — one community, one repo.
#   * No thresholds are enforced on RSS growth; the numbers are recorded as
#     evidence for a human, because a threshold picked without a baseline is
#     just a flaky test waiting to happen.
# =============================================================================
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="gate:soak"
# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"
# shellcheck source=lib/candidate.sh
source "${GATES_DIR}/lib/candidate.sh"
# shellcheck source=lib/harness.sh
source "${GATES_DIR}/lib/harness.sh"
# shellcheck source=lib/workload.sh
source "${GATES_DIR}/lib/workload.sh"

EVIDENCE="${GATES_EVIDENCE:-/tmp/buzz-gates/adhoc/soak}"
PROFILE="${GATES_PROFILE:-ci}"
DURATION="${GATES_SOAK_DURATION:-300}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --execute)        GATES_EXECUTE=1; shift ;;
    --dry-run)        GATES_EXECUTE=0; shift ;;
    --evidence)       EVIDENCE="$2"; shift 2 ;;
    --profile)        PROFILE="$2"; shift 2 ;;
    --project-name)   GATES_PROJECT="$2"; shift 2 ;;
    --soak-duration)  DURATION="$2"; shift 2 ;;
    *) err "unknown option: $1"; exit 2 ;;
  esac
done

require_jq || exit 2
mkdir -p "${EVIDENCE}"
cd "${REPO_ROOT}"
STARTED="$(epoch_s)"
GATES_HARNESS_LOG="${EVIDENCE}/harness.log"
TARGET_DIR="$(profile_target_dir "${PROFILE}")"
ACP_LOG="${EVIDENCE}/acp.log"
RELAY_LOG="${EVIDENCE}/relay.log"
RSS_CSV="${EVIDENCE}/rss.csv"

section "GATE 4/4 · soak — candidate × candidate, ${DURATION}s"

if [[ "${DURATION}" -lt 300 ]]; then
  warn "soak duration ${DURATION}s is below the 300s default — this proves very little."
fi
if [[ "${DURATION}" -le 900 ]]; then
  note "SHORT SOAK. Real pre-release soaks run for hours (--soak-duration 14400)."
fi

harness_guard || exit 2
harness_arm_teardown

harness_up     || { record_result "${EVIDENCE}" soak fail "${STARTED}" '{"error":"harness bring-up failed"}'; print_result_line soak fail 0 "${EVIDENCE}"; exit 1; }
harness_schema || true
harness_seed   || true
harness_relay_build "${PROFILE}" || true
harness_relay_start "$(candidate_bin relay "${REPO_ROOT}" "${PROFILE}")" "${RELAY_LOG}" || true

step "Mint throwaway keypairs and start candidate buzz-acp"
preview "$(candidate_bin acp "${REPO_ROOT}" "${PROFILE}")"
note "same stub agent + project-routing config the skew gate uses (lib/workload.sh)"

step "Loop the synthetic workload for ${DURATION}s, sampling RSS each iteration"
note "per iteration: announce repo -> open @mentioning issue -> await stub reply"
note "RSS samples -> ${RSS_CSV}; failures abort the loop and fail the gate"

step "Assert across the whole run: no ERROR lines, every dropped counter 0, clean shutdown"

if is_dry; then
  record_result "${EVIDENCE}" soak dry-run "${STARTED}" \
    "$(jq -n --argjson d "${DURATION}" '{duration_s:$d, note:"planned only"}')"
  print_result_line soak dry-run 0 "${EVIDENCE}"
  exit 0
fi

# ---- execute ----------------------------------------------------------------

FAILURES=()
ITERATIONS=0
ITERATIONS_OK=0
CLI="$(candidate_bin cli "${REPO_ROOT}" "${PROFILE}")"
echo "iteration,epoch,acp_rss_kb,relay_rss_kb" > "${RSS_CSV}"

keys="$(workload_generate_key)" || FAILURES+=("keygen-agent")
IFS=$'\t' read -r AGENT_SEC AGENT_PUB <<< "${keys:-$'\t'}"
keys="$(workload_generate_key)" || FAILURES+=("keygen-driver")
IFS=$'\t' read -r DRIVER_SEC DRIVER_PUB <<< "${keys:-$'\t'}"

ACP_PID=""
if [[ ${#FAILURES[@]} -eq 0 ]]; then
  ACP_PID="$(workload_start_acp "$(candidate_bin acp "${REPO_ROOT}" "${PROFILE}")" \
              "${ACP_LOG}" "${AGENT_SEC}" "${EVIDENCE}/state" "${DRIVER_PUB}")" || FAILURES+=("acp-start")
  wait_for_marker "${ACP_LOG}" "connected to relay at" 90 || FAILURES+=("marker:connected")
fi

rss_kb() { [[ -n "${1:-}" ]] && ps -o rss= -p "$1" 2>/dev/null | tr -d ' ' || echo ""; }

DEADLINE=$(( $(epoch_s) + DURATION ))
while [[ ${#FAILURES[@]} -eq 0 ]] && (( $(epoch_s) < DEADLINE )); do
  ITERATIONS=$(( ITERATIONS + 1 ))
  repo_id="gates-soak-${ITERATIONS}-$(date -u +%s)"

  if workload_announce_repo "${CLI}" "${AGENT_SEC}" "${repo_id}" >>"${EVIDENCE}/workload.log" 2>&1 \
     && root="$(workload_open_issue "${CLI}" "${DRIVER_SEC}" "${AGENT_PUB}" "${repo_id}")" \
     && [[ -n "${root}" ]] \
     && workload_wait_reply "${CLI}" "${DRIVER_SEC}" "${root}" 120; then
    ITERATIONS_OK=$(( ITERATIONS_OK + 1 ))
  else
    FAILURES+=("iteration-${ITERATIONS}-failed")
  fi

  printf '%s,%s,%s,%s\n' "${ITERATIONS}" "$(epoch_s)" \
    "$(rss_kb "${ACP_PID}")" "$(rss_kb "${GATES_RELAY_PID:-}")" >> "${RSS_CSV}"

  log "iteration ${ITERATIONS} done (${ITERATIONS_OK} ok), $(( DEADLINE - $(epoch_s) ))s remaining"
done

[[ -n "${ACP_PID}" ]] && { workload_shutdown_acp "${ACP_PID}" "${ACP_LOG}" 45 || FAILURES+=("unclean-shutdown"); }
harness_relay_stop

assert_dropped_zero   "${ACP_LOG}"   || FAILURES+=("dropped-nonzero")
assert_no_error_lines "${ACP_LOG}"   || FAILURES+=("acp-error-lines")
assert_no_error_lines "${RELAY_LOG}" || FAILURES+=("relay-error-lines")

RESULT=pass
[[ ${#FAILURES[@]} -eq 0 && ${ITERATIONS_OK} -gt 0 ]] || RESULT=fail

# RSS drift, first vs last sample — recorded, not thresholded (see header).
RSS_FIRST="$(awk -F, 'NR==2{print $3}' "${RSS_CSV}")"
RSS_LAST="$(awk -F, 'END{print $3}' "${RSS_CSV}")"

record_result "${EVIDENCE}" soak "${RESULT}" "${STARTED}" \
  "$(jq -n --argjson d "${DURATION}" --argjson it "${ITERATIONS}" --argjson ok "${ITERATIONS_OK}" \
      --arg rf "${RSS_FIRST:-}" --arg rl "${RSS_LAST:-}" \
      --argjson f "$(printf '%s\n' "${FAILURES[@]:-}" | jq -R . | jq -s 'map(select(length>0))')" \
    '{duration_s:$d, iterations:$it, iterations_ok:$ok, failures:$f,
      acp_rss_kb:{first:$rf, last:$rl},
      soak_class:(if $d >= 14400 then "release-grade" elif $d >= 900 then "extended" else "smoke (CI-of-one default; NOT a release soak)" end),
      proves:"the candidate pairing sustains the synthetic workload for the recorded duration with zero dropped counters and no ERROR lines",
      does_not_prove:"concurrency/throughput behaviour, slow leaks beyond the recorded duration, or multi-tenant load"}')"

DURATION_S=$(( $(epoch_s) - STARTED ))
print_result_line soak "${RESULT}" "${DURATION_S}" "${EVIDENCE}"
[[ "${RESULT}" == "pass" ]]
