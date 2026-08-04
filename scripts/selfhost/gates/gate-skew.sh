#!/usr/bin/env bash
# =============================================================================
# gate-skew.sh — GATE 3: version-skew matrix.
# =============================================================================
# WHAT THIS GATE PROVES
#   A rolling deploy is never atomic: for some window, a candidate relay talks
#   to deployed agents, and deployed relays talk to candidate agents. This gate
#   boots both mixed pairings against the isolated harness and asserts each one
#   completes a full agent lifecycle:
#
#     pairing A  relay@candidate  ×  acp@deployed   (/opt/buzz/bin/buzz-acp)
#     pairing B  relay@deployed   ×  acp@candidate  (buzz-local:unified-*)
#
#   Per pairing, all of these must hold:
#     boots            — the harness process starts and the ACP handshake lands
#     connects         — "connected to relay at ..."          (lib.rs:1925)
#     discovers        — "discovered N channel(s)"            (lib.rs:2053)
#                        "discovered repository"              (lib.rs:4695)
#     enrols (fresh)   — "enrolment history reconstruction complete"
#                                                             (relay.rs:6439)
#                        "root history reconstruction complete"
#                                                             (relay.rs:6441)
#     answers          — a stub-agent reply lands on the root
#     clean shutdown   — SIGTERM -> "buzz-acp stopped"        (lib.rs:3641)
#     dropped=0        — no lagged/shed counters              (lib.rs:887,:985)
#     no ERROR lines
#
# WHAT IT DELIBERATELY DOES NOT PROVE
#   * NOT agent quality. The turn is answered by gates/acp-stub-agent.mjs, a
#     deterministic stub. This gate is about the wire contract between relay and
#     harness across versions, not about what an LLM says. Using a real agent
#     would make the gate non-deterministic and billable, which is worse than
#     narrow.
#   * NOT the full protocol surface. One repo, one issue, one turn. A skew bug
#     in, say, media upload or voice would sail through. Widening the workload
#     is the obvious next increment (lib/workload.sh is the single place to do
#     it, shared with the soak gate).
#   * NOT candidate×candidate. That pairing is what gates 1, 2 and 4 already
#     exercise; the matrix here is only the MIXED cells, which is where skew
#     bugs live.
#   * NOT a downgrade test. Nothing here proves the deployed version can read
#     data the candidate wrote and then be rolled back.
#
# LIVE-RUN STATUS
#   See gates/README.md "What has actually been executed". The pairings are
#   implemented end to end; as of the commit that introduced this file they had
#   not yet been run green on this host, and the marker set above is derived
#   from source rather than from an observed transcript.
# =============================================================================
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="gate:skew"
# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"
# shellcheck source=lib/candidate.sh
source "${GATES_DIR}/lib/candidate.sh"
# shellcheck source=lib/harness.sh
source "${GATES_DIR}/lib/harness.sh"
# shellcheck source=lib/workload.sh
source "${GATES_DIR}/lib/workload.sh"

EVIDENCE="${GATES_EVIDENCE:-/tmp/buzz-gates/adhoc/skew}"
PROFILE="${GATES_PROFILE:-ci}"
DEPLOYED_ACP="${GATES_DEPLOYED_ACP:-/opt/buzz/bin/buzz-acp}"
ONLY_PAIRING=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --execute)      GATES_EXECUTE=1; shift ;;
    --dry-run)      GATES_EXECUTE=0; shift ;;
    --evidence)     EVIDENCE="$2"; shift 2 ;;
    --profile)      PROFILE="$2"; shift 2 ;;
    --project-name) GATES_PROJECT="$2"; shift 2 ;;
    --only)         ONLY_PAIRING="$2"; shift 2 ;;
    *) err "unknown option: $1"; exit 2 ;;
  esac
done

require_jq || exit 2
mkdir -p "${EVIDENCE}"
cd "${REPO_ROOT}"
STARTED="$(epoch_s)"
GATES_HARNESS_LOG="${EVIDENCE}/harness.log"
TARGET_DIR="$(profile_target_dir "${PROFILE}")"
PAIRING_RESULTS='[]'

section "GATE 3/4 · skew matrix — (relay@candidate × acp@deployed), (relay@deployed × acp@candidate)"

harness_guard || exit 2
harness_arm_teardown

# start_relay_candidate — build and run the candidate relay from source, exactly
# the from-source pattern scripts/start-isolated-test-relay.sh establishes.
start_relay_candidate() {
  harness_relay_build "${PROFILE}" || return 1
  harness_relay_start "$(candidate_bin relay "${REPO_ROOT}" "${PROFILE}")" "${EVIDENCE}/relay-candidate.log"
}

# start_relay_deployed — run the deployed image as a container on the gates
# compose network (profile "deployed-relay" in docker-compose.gates.yml).
start_relay_deployed() {
  runx "Start DEPLOYED relay image ${GATES_DEPLOYED_IMAGE:-buzz-local:unified-13acbaf2} on :${GATES_RELAY_PORT}" \
    -- docker compose -p "${GATES_PROJECT}" \
       -f "${REPO_ROOT}/docker-compose.harness.yml" \
       -f "${REPO_ROOT}/scripts/selfhost/gates/docker-compose.gates.yml" \
       --profile deployed-relay up -d deployed-relay || return 1
  step "Wait for the deployed relay to accept connections (<=60s)"
  is_dry && return 0
  local i
  for i in $(seq 1 60); do
    curl -sf -o /dev/null "$(harness_relay_url)/" 2>/dev/null && { ok "Deployed relay live"; return 0; }
    sleep 1
  done
  err "Deployed relay did not come up on :${GATES_RELAY_PORT}"
  harness_compose logs --tail 40 deployed-relay > "${EVIDENCE}/relay-deployed.log" 2>&1 || true
  return 1
}

stop_relay_deployed() {
  is_dry && return 0
  harness_compose logs --no-color deployed-relay > "${EVIDENCE}/relay-deployed.log" 2>&1 || true
  harness_compose --profile deployed-relay stop deployed-relay >/dev/null 2>&1 || true
  harness_compose --profile deployed-relay rm -f deployed-relay >/dev/null 2>&1 || true
}

# run_pairing <label> <relay_side: candidate|deployed> <acp_binary>
run_pairing() {
  local label="$1" relay_side="$2" acp_bin="$3"
  local dir="${EVIDENCE}/${label}"
  local acp_log="${dir}/acp.log"
  local failures=()
  mkdir -p "${dir}"

  section "pairing ${label} — relay@${relay_side} × acp@$( [[ "${relay_side}" == candidate ]] && echo deployed || echo candidate )"

  # Fresh database per pairing: a root enrolled by the previous pairing would
  # make "enrols on a FRESH root" a lie.
  harness_schema || { failures+=("schema"); }
  harness_seed   || { failures+=("seed"); }

  if [[ "${relay_side}" == "candidate" ]]; then
    start_relay_candidate || failures+=("relay-start")
  else
    start_relay_deployed  || failures+=("relay-start")
  fi

  step "Mint throwaway agent + driver keypairs (per-run, never persisted)"
  preview cargo run --quiet -p buzz-admin -- generate-key
  note "nothing under /opt/buzz/keys is read or written"

  step "Start buzz-acp: ${acp_bin}"
  note "BUZZ_ACP_AGENT_COMMAND=node ${GATES_STUB_AGENT} (deterministic stub; goose is not installed here)"
  note "BUZZ_ACP_PROJECT_ROUTING_ENABLED=true, BUZZ_ACP_DISPLAY_NAME=${GATES_AGENT_DISPLAY_NAME}"

  step "Drive workload: announce repo -> open issue @mentioning the agent -> await reply"
  note "explicit @mention is REQUIRED: an unknown root only enrols on Addressing::ExplicitMention (project.rs:7892)"

  step "Assert markers, then SIGTERM and assert clean shutdown"
  note "markers: connected to relay at / discovered N channel(s) / enrolment history reconstruction complete /"
  note "         root history reconstruction complete / buzz-acp stopped; plus dropped=0 and no ERROR lines"

  if is_dry; then
    PAIRING_RESULTS="$(jq -n --argjson acc "${PAIRING_RESULTS}" --arg l "${label}" \
      '$acc + [{pairing:$l, result:"dry-run", failures:[]}]')"
    [[ "${relay_side}" == "candidate" ]] && harness_relay_stop || stop_relay_deployed
    return 0
  fi

  local keys agent_sec agent_pub driver_sec driver_pub acp_pid root repo_id
  # Staged candidate CLI when run-gates.sh staged one; target/ otherwise.
  local cli; cli="$(candidate_bin cli "${REPO_ROOT}" "${PROFILE}")"

  if keys="$(workload_generate_key)"; then
    IFS=$'\t' read -r agent_sec agent_pub <<< "${keys}"
  else
    failures+=("keygen-agent")
  fi
  if keys="$(workload_generate_key)"; then
    IFS=$'\t' read -r driver_sec driver_pub <<< "${keys}"
  else
    failures+=("keygen-driver")
  fi

  if [[ ${#failures[@]} -eq 0 ]]; then
    repo_id="gates-probe-$(date -u +%s)"

    # Announce the repository BEFORE the agent boots. Boot-time discovery is
    # the path every production agent actually takes (existing announcements
    # are read at startup, and "enrolment history reconstruction complete" is
    # only logged once there is something to reconstruct against) — the first
    # live run proved a boot-then-announce ordering leaves the runtime with
    # zero repositories, no reconstruction line, and a 90s marker stall that
    # pushed every later step past its window.
    workload_announce_repo "${cli}" "${agent_sec}" "${repo_id}" \
      > "${dir}/repo-create.log" 2>&1 || failures+=("repo-announce")

    acp_pid="$(workload_start_acp "${acp_bin}" "${acp_log}" "${agent_sec}" "${dir}/state" "${driver_pub}" "${cli}")" \
      || failures+=("acp-start")

    wait_for_marker "${acp_log}" "connected to relay at" 90 \
      || failures+=("marker:connected")
    wait_for_marker "${acp_log}" "channel(s)" 60 \
      || failures+=("marker:discovered-channels")
    wait_for_marker "${acp_log}" "discovered repository" 60 \
      || failures+=("marker:discovered-repository")
    wait_for_marker "${acp_log}" "enrolment history reconstruction complete" 90 \
      || failures+=("marker:enrolment-history")

    if root="$(workload_open_issue "${cli}" "${driver_sec}" "${agent_pub}" "${repo_id}")" \
       && [[ -n "${root}" ]]; then
      echo "${root}" > "${dir}/root-event-id.txt"
      # A FRESH root has no history to reconstruct, so the reconstruction line
      # never prints for it — the line that proves enrolment-with-a-turn on a
      # new root is the queue admission itself (run 6's debug trace: queued=true
      # one second after the issue landed, then agent_claimed).
      wait_for_marker "${acp_log}" "project event queued for a turn" 90 \
        || failures+=("marker:fresh-root-enrolment")
      workload_wait_reply "${cli}" "${driver_sec}" "${root}" 120 \
        || failures+=("no-agent-reply")
    else
      failures+=("issue-create")
    fi

    if [[ -n "${acp_pid:-}" ]]; then
      workload_shutdown_acp "${acp_pid}" "${acp_log}" 45 || failures+=("unclean-shutdown")
    fi

    assert_dropped_zero    "${acp_log}" || failures+=("dropped-nonzero")
    assert_no_error_lines  "${acp_log}" || failures+=("acp-error-lines")
  fi

  if [[ "${relay_side}" == "candidate" ]]; then
    harness_relay_stop
    assert_no_error_lines "${EVIDENCE}/relay-candidate.log" || failures+=("relay-error-lines")
  else
    stop_relay_deployed
    assert_no_error_lines "${EVIDENCE}/relay-deployed.log" || failures+=("relay-error-lines")
  fi

  local presult=pass
  [[ ${#failures[@]} -eq 0 ]] || presult=fail
  PAIRING_RESULTS="$(jq -n --argjson acc "${PAIRING_RESULTS}" \
    --arg l "${label}" --arg r "${presult}" \
    --argjson f "$(printf '%s\n' "${failures[@]:-}" | jq -R . | jq -s 'map(select(length>0))')" \
    '$acc + [{pairing:$l, result:$r, failures:$f}]')"

  if [[ "${presult}" == "pass" ]]; then ok "pairing ${label}: PASS"; else err "pairing ${label}: FAIL — ${failures[*]}"; fi
}

# ---- preconditions ----------------------------------------------------------

if ! is_dry; then
  [[ -r "${DEPLOYED_ACP}" ]] || { err "deployed acp not readable: ${DEPLOYED_ACP} (needed for pairing A)"; }
  docker image inspect "${GATES_DEPLOYED_IMAGE:-buzz-local:unified-13acbaf2}" >/dev/null 2>&1 \
    || err "deployed relay image missing: ${GATES_DEPLOYED_IMAGE:-buzz-local:unified-13acbaf2} (needed for pairing B)"
fi

harness_up || { record_result "${EVIDENCE}" skew fail "${STARTED}" '{"error":"harness bring-up failed"}'; print_result_line skew fail 0 "${EVIDENCE}"; exit 1; }

[[ -z "${ONLY_PAIRING}" || "${ONLY_PAIRING}" == "A" ]] && \
  run_pairing "A-relay-candidate-acp-deployed" candidate "${DEPLOYED_ACP}"
[[ -z "${ONLY_PAIRING}" || "${ONLY_PAIRING}" == "B" ]] && \
  run_pairing "B-relay-deployed-acp-candidate" deployed "$(candidate_bin acp "${REPO_ROOT}" "${PROFILE}")"

# ---- verdict ----------------------------------------------------------------

if is_dry; then
  record_result "${EVIDENCE}" skew dry-run "${STARTED}" \
    "$(jq -n --argjson p "${PAIRING_RESULTS}" '{pairings:$p, note:"planned only"}')"
  print_result_line skew dry-run 0 "${EVIDENCE}"
  exit 0
fi

RESULT=pass
[[ "$(echo "${PAIRING_RESULTS}" | jq '[.[] | select(.result != "pass")] | length')" -eq 0 ]] || RESULT=fail

record_result "${EVIDENCE}" skew "${RESULT}" "${STARTED}" \
  "$(jq -n --argjson p "${PAIRING_RESULTS}" \
    '{pairings:$p,
      proves:"both mixed relay/acp version pairings complete boot -> connect -> discover -> fresh-root enrol -> one answered comment -> clean shutdown, with dropped=0 and no ERROR lines",
      does_not_prove:"agent output quality (a deterministic stub answers), protocol surfaces beyond one issue/one turn, or rollback compatibility"}')"

DURATION=$(( $(epoch_s) - STARTED ))
print_result_line skew "${RESULT}" "${DURATION}" "${EVIDENCE}"
[[ "${RESULT}" == "pass" ]]
