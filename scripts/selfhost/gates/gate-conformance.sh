#!/usr/bin/env bash
# =============================================================================
# gate-conformance.sh — GATE 2: TLA+ trace conformance.
# =============================================================================
# The north star from crates/buzz-conformance/src/lib.rs:6-7 —
#   "don't ask 'did the model pass'; ask 'did the running code emit a trace
#    the model accepts.'"
#
# PHASE A — checker integrity  [REAL]
#   `cargo test -p buzz-conformance --all-targets`. This is not decorative:
#   the crate ships adversarial replay fixtures (tests/fixtures/*.jsonl) that
#   assert check_trace FAILS on a host/channel fence skip, on a foreign-row
#   leak, and on a coverage breach, plus proptests over the checker, plus the
#   unit tests of the `check-trace` binary phase B invokes. Phase A proves the
#   judge still bites. If the checker were silently reduced to `Ok(())`, phase
#   A is what catches it.
#   buzz-conformance depends on no production buzz crate (see its Cargo.toml
#   "Independence rule"), so it builds and tests cleanly on this host — the
#   openssl gap that blocks buzz-relay's test targets does not touch it.
#
# PHASE B — live trace replay  [REAL]
#   Stands up the isolated harness, runs the CANDIDATE relay from source with
#   BUZZ_CONFORMANCE_TRACE_PATH pointed at a file under this run's evidence
#   directory, drives the shared gate workload against it, stops the relay
#   cleanly, and replays the JSONL the relay actually wrote through
#   `check-trace` (crates/buzz-conformance/src/bin/check-trace.rs).
#
#   This is the half that says something about THIS relay. Phase A says the
#   judge is sound; phase B puts the candidate in front of it.
#
#   Wiring this depends on, verified before the run by `probe_emission_wiring`
#   (see below) and again at runtime by the trace file's own existence:
#     W1. BUZZ_CONFORMANCE_TRACE_PATH selects a JsonlTracer instead of the
#         NoopTracer — crates/buzz-relay/src/config.rs (parse) and
#         crates/buzz-relay/src/state.rs (bind, via
#         crate::conformance::tracer_for_trace_path).
#     W2. A shell-invokable replay entrypoint —
#         crates/buzz-conformance/src/bin/check-trace.rs.
#   Both were BLOCKED when this gate was first written; the probe that used to
#   report them now asserts they are present and FAILS the gate if either has
#   gone missing. A half-wired tree can never quietly pass: with the relay's
#   switch reverted the gate reports the missing wiring by file, and with the
#   switch present but the workload silent it reports the checker's own
#   empty-trace coverage breach. There is no path to green that does not go
#   through a real trace.
#
# WHAT PHASE B DELIBERATELY DOES NOT DO
#   * It does NOT start buzz-acp. The scenario driven here is the RELAY's
#     ingest and read seams — write an event, read it back — because those are
#     the seams the trace schema covers (crates/buzz-relay/src/handlers/
#     ingest.rs and req.rs are the only emit sites). Booting the ACP stub
#     agent would add an agent-harness lifecycle that proves nothing about
#     MultiTenantRelay.tla while giving this gate a second, unrelated way to
#     go red. The agent lifecycle is gate 3 (skew) and gate 4 (soak); the
#     write/read halves of the same shared scenario are reused from
#     lib/workload.sh rather than re-cut here.
#   * The read half deliberately goes over the WEBSOCKET, not the `buzz` CLI's
#     HTTP bridge. The bridge (api/bridge.rs) is a second read path that reuses
#     req.rs's query builders but has its own delivery loop and NO trace emit
#     site, so a bridge-only workload yields a trace with zero read
#     observations — the first live run of this gate captured exactly that.
#     lib/workload.sh:workload_read_subscription drives the traced path with
#     buzz-test-cli, and `read_message_rows` is a required action so the gap
#     can never quietly come back.
#   * It does NOT prove the relay is spec-conformant in general. Trace
#     conformance only ever judges the executions you actually ran — the
#     crate's own docs are explicit that this is "not a proof". One repo
#     announce, one issue, one history read is a narrow trace. Widening it is
#     the obvious next increment, and lib/workload.sh is the one place to do it.
#   * Under the default --group-by state, `state_mismatch` is not exercised by
#     the live trace (the partition key is the tuple that check compares); that
#     mode is carried by phase A's fixtures. See the check-trace module docs.
# =============================================================================
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="gate:conformance"
# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"
# shellcheck source=lib/candidate.sh
source "${GATES_DIR}/lib/candidate.sh"
# shellcheck source=lib/harness.sh
source "${GATES_DIR}/lib/harness.sh"
# shellcheck source=lib/workload.sh
source "${GATES_DIR}/lib/workload.sh"

EVIDENCE="${GATES_EVIDENCE:-/tmp/buzz-gates/adhoc/conformance}"
PROFILE="${GATES_PROFILE:-ci}"
TRACE_ENV_VAR="BUZZ_CONFORMANCE_TRACE_PATH"

# Critical action kinds the phase-B workload MUST exercise. Empty would make
# the coverage-breach mode near-vacuous (only impl_bug and the empty trace would
# bite) and the crate is blunt that coverage breach is what stops trace
# conformance being decorative logging — so this gate always names a set.
#
# The default is the set this exact scenario provably drives, each entry tied to
# the emit site that produces it:
#   write_insert_global  — the channel-less writes (repo announce kind:30617,
#                          issue kind:1621) landing at ingest.rs's
#                          dispatch_persistent_event seam.
#   read_message_rows    — the WebSocket REQ coming back through
#                          handlers/req.rs's row-projection seam. NOT the `buzz`
#                          CLI's read: that goes through the HTTP bridge, which
#                          is a different, untraced read path. The first live
#                          run of this gate proved the point by capturing two
#                          writes and zero reads, and this requirement is what
#                          turned that into a red gate instead of a green one.
# Deliberately NOT required, because this scenario cannot drive them and a
# requirement nothing satisfies is just a broken gate:
#   auth_check           — needs a channel-scoped REQ; the WS probe's filter is
#                          channel-less (`#e` only), so no membership decision
#                          is reached.
#   write_insert /        — need channel-bearing writes; the project-routing
#   write_duplicate         lane this workload drives is channel-less.
#   read_by_id_rows      — the NIP-50 search lane, which nothing here exercises.
#   sanitized_error      — a rejected request; this workload is a happy path.
# If a regression deletes either required emit site the trace gets shorter and
# this gate goes red, which is the entire point of declaring them. Widen the
# list when the workload widens; override for an ad-hoc run with --require.
GATES_CONFORMANCE_REQUIRE="${GATES_CONFORMANCE_REQUIRE:-write_insert_global,read_message_rows}"
GATES_CONFORMANCE_GROUP_BY="${GATES_CONFORMANCE_GROUP_BY:-state}"
SKIP_PHASE_B=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --execute)      GATES_EXECUTE=1; shift ;;
    --dry-run)      GATES_EXECUTE=0; shift ;;
    --evidence)     EVIDENCE="$2"; shift 2 ;;
    --profile)      PROFILE="$2"; shift 2 ;;
    --project-name) GATES_PROJECT="$2"; shift 2 ;;
    --require)      GATES_CONFORMANCE_REQUIRE="$2"; shift 2 ;;
    --group-by)     GATES_CONFORMANCE_GROUP_BY="$2"; shift 2 ;;
    --skip-phase-b) SKIP_PHASE_B=1; shift ;;
    *) err "unknown option: $1"; exit 2 ;;
  esac
done

require_jq || exit 2
mkdir -p "${EVIDENCE}"
cd "${REPO_ROOT}"
STARTED="$(epoch_s)"
GATES_HARNESS_LOG="${EVIDENCE}/harness.log"
TRACE_FILE="${EVIDENCE}/relay-trace.jsonl"
CHECK_JSON="${EVIDENCE}/check-trace.json"
CHECK_TXT="${EVIDENCE}/check-trace.txt"
PHASE_B_FAILURES=()
PHASE_B_NOTE=""

section "GATE 2/4 · conformance — TLA+ trace replay (MultiTenantRelay.tla)"

# ---- Wiring probe -----------------------------------------------------------
# This probe used to REPORT the two blockers that stopped phase B existing. Its
# job is now inverted: assert the wiring is present before we build anything, and
# refuse to run — loudly, never silently green — if any of it has gone away.
#
# Located dynamically so the message cites a live file:line rather than a number
# that rots the moment someone edits state.rs.

probe_emission_wiring() {
  WIRING_MISSING=()

  local runtime_binding hardcoded_noop
  # W1a — something in the relay outside tracers.rs must know the variable.
  runtime_binding="$(grep -rl "${TRACE_ENV_VAR}\|tracer_for_trace_path" \
                       crates/buzz-relay/src --include='*.rs' 2>/dev/null \
                     | grep -v 'conformance/tracers.rs' || true)"
  if [[ -z "${runtime_binding}" ]]; then
    WIRING_MISSING+=("W1 relay switch: no ${TRACE_ENV_VAR} handling anywhere in crates/buzz-relay/src outside conformance/tracers.rs — the relay cannot be asked to emit a trace from outside the process.")
  fi

  # W1b — and the unconditional NoopTracer binding must be gone. A tree that
  # parses the variable but still hardcodes the no-op is exactly the half-wired
  # state this probe exists to refuse.
  hardcoded_noop="$(grep -n 'tracer: Arc::new(crate::conformance::NoopTracer)' \
                      crates/buzz-relay/src/state.rs 2>/dev/null | head -1 | cut -d: -f1)"
  if [[ -n "${hardcoded_noop}" ]]; then
    WIRING_MISSING+=("W1 relay switch: crates/buzz-relay/src/state.rs:${hardcoded_noop} still binds NoopTracer unconditionally — ${TRACE_ENV_VAR} would be parsed and ignored.")
  fi

  # W2 — a shell-invokable replay entrypoint.
  if [[ ! -f crates/buzz-conformance/src/bin/check-trace.rs ]] \
     && ! grep -qE '^\[\[(bin|example)\]\]' crates/buzz-conformance/Cargo.toml 2>/dev/null; then
    WIRING_MISSING+=("W2 replay entrypoint: crates/buzz-conformance has no src/bin/check-trace.rs and declares no [[bin]] — check_trace (crates/buzz-conformance/src/checker.rs:74) has nothing a shell gate can invoke on a captured .jsonl.")
  fi

  [[ ${#WIRING_MISSING[@]} -eq 0 ]]
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
note "and that the check-trace binary phase B invokes reaches the same verdicts"

if ! is_dry; then
  set +e
  cargo test -p buzz-conformance --all-targets > >(tee "${EVIDENCE}/conformance-test.log") 2>&1
  rc=${PIPESTATUS[0]}
  set -e
  if [[ ${rc} -eq 0 ]]; then PHASE_A=pass; ok "Phase A green"; else PHASE_A=fail; err "Phase A FAILED — the trace checker itself is broken"; fi
fi

# ---- Phase B ----------------------------------------------------------------

PHASE_B=skipped
CHECKER_BIN=""

# phase_b_drive — everything between a running harness and a replayed verdict.
# Appends to PHASE_B_FAILURES rather than returning early on the first problem,
# so one transcript shows every broken link instead of only the first.
phase_b_drive() {
  local cli ws_cli relay_bin keys driver_sec driver_pub agent_sec agent_pub repo_id root
  local wdir="${EVIDENCE}/workload"
  mkdir -p "${wdir}"

  harness_schema || { PHASE_B_FAILURES+=("schema"); return 1; }
  harness_seed   || { PHASE_B_FAILURES+=("seed"); return 1; }

  harness_relay_build "${PROFILE}" || { PHASE_B_FAILURES+=("relay-build"); return 1; }
  relay_bin="$(candidate_bin relay "${REPO_ROOT}" "${PROFILE}")"

  # The CLI is the workload's write driver. run-gates.sh builds and stages it
  # before the gates run; a standalone invocation has to build it itself.
  cli="$(candidate_bin cli "${REPO_ROOT}" "${PROFILE}")"
  if [[ ! -x "${cli}" ]]; then
    runx "Build workload driver (buzz CLI, profile=${PROFILE})" \
      -- cargo build --profile "${PROFILE}" -p buzz-cli || { PHASE_B_FAILURES+=("cli-build"); return 1; }
  fi

  # buzz-test-cli is the WS read driver — a PROBE, never staged and never
  # hash-bound, because nothing ships it. It is needed because the `buzz` CLI
  # reads over the HTTP bridge, which is not the traced read path; see
  # lib/workload.sh:workload_read_subscription for the full reasoning. Only the
  # one bin is built: the crate's `mention` bin does not compile on this host
  # (a rustls `ring` feature gap unrelated to anything here).
  ws_cli="${REPO_ROOT}/target/$(profile_target_dir "${PROFILE}")/buzz-test-cli"
  if [[ ! -x "${ws_cli}" ]]; then
    runx "Build WS read probe (buzz-test-cli, profile=${PROFILE})" \
      -- cargo build --profile "${PROFILE}" -p buzz-test-client --bin buzz-test-cli \
      || { PHASE_B_FAILURES+=("ws-probe-build"); return 1; }
  fi

  step "Point the relay's conformance tracer at ${TRACE_FILE}"
  note "${TRACE_ENV_VAR} is read by Config::from_env and bound in AppState::new;"
  note "unset (production) that binding is NoopTracer and no file is opened."
  is_dry || rm -f "${TRACE_FILE}"

  harness_relay_start "${relay_bin}" "${EVIDENCE}/relay.log" \
    "${TRACE_ENV_VAR}=${TRACE_FILE}" \
    || { PHASE_B_FAILURES+=("relay-start"); return 1; }

  step "Mint a throwaway driver keypair (per-run, never persisted)"
  preview cargo run --quiet -p buzz-admin -- generate-key
  note "nothing under /opt/buzz/keys is read or written"

  step "Drive the shared workload's relay-facing half against the traced relay"
  note "writes: announce repo (kind:30617) -> open issue (kind:1621) -> comment on root (kind:1)"
  note "reads:  root history, then one WebSocket REQ per identity (#e=<root>, kind:1)"
  note "the WS leg is load-bearing: the buzz CLI reads over the HTTP bridge, which is not traced"
  note "the comment is load-bearing too: it is what makes the read return rows to confine"
  note "reused from lib/workload.sh — the same scenario gates 3 and 4 drive, minus the ACP agent"

  if ! is_dry; then
    if keys="$(workload_generate_key)"; then
      IFS=$'\t' read -r agent_sec agent_pub <<< "${keys}"
    else
      PHASE_B_FAILURES+=("keygen-agent")
    fi
    if keys="$(workload_generate_key)"; then
      IFS=$'\t' read -r driver_sec driver_pub <<< "${keys}"
    else
      PHASE_B_FAILURES+=("keygen-driver")
    fi

    if [[ -n "${agent_sec:-}" && -n "${driver_sec:-}" ]]; then
      repo_id="gates-conformance-$(date -u +%s)"
      workload_announce_repo "${cli}" "${agent_sec}" "${repo_id}" \
        > "${wdir}/repo-create.log" 2>&1 || PHASE_B_FAILURES+=("repo-announce")

      if root="$(workload_open_issue "${cli}" "${driver_sec}" "${agent_pub}" "${repo_id}")" \
         && [[ -n "${root}" ]]; then
        echo "${root}" > "${wdir}/root-event-id.txt"

        # A kind:1 comment e-tagging the root. Third write, and — because the
        # WS probe filters on `#e=<root>` — the one event the reads below can
        # actually match. Without it every read step carries
        # `row_communities: []`, which satisfies Inv_NonInterference
        # vacuously: the confinement check would have nothing to confine.
        workload_comment_on_root "${cli}" "${agent_sec}" "${agent_pub}" "${repo_id}" "${root}" \
          > "${wdir}/comment.log" 2>&1 || PHASE_B_FAILURES+=("comment")

        # Two readers, two identities: the driver reads its own root and the
        # announcing key reads it too. Two distinct actors is not padding — it
        # is what makes the replay exercise more than one trace partition,
        # which is the shape a live trace actually has.
        workload_read_history "${cli}" "${driver_sec}" "${root}" > "${wdir}/history-driver.log" 2>&1
        workload_read_history "${cli}" "${agent_sec}"  "${root}" > "${wdir}/history-agent.log"  2>&1

        # The traced read. Same two identities over the WebSocket REQ path,
        # which is where handle_req's row-projection emit site lives. kind:1
        # so the comment above comes back and the observation carries rows.
        workload_read_subscription "${ws_cli}" "${driver_sec}" 1 "${root}" \
          "${wdir}/req-driver.log" 30 || PHASE_B_FAILURES+=("ws-req-driver")
        workload_read_subscription "${ws_cli}" "${agent_sec}" 1 "${root}" \
          "${wdir}/req-agent.log" 30 || PHASE_B_FAILURES+=("ws-req-agent")
      else
        PHASE_B_FAILURES+=("issue-create")
      fi
    fi
  fi

  step "Stop the relay cleanly (SIGTERM), then replay what it wrote"
  note "JsonlTracer flushes per line, so a clean stop leaves no partial line"
  note "then assert the file exists — a relay that ignored ${TRACE_ENV_VAR} leaves none"
  harness_relay_stop
  is_dry && return 0

  # The runtime half of the wiring assertion. A relay that ignored
  # ${TRACE_ENV_VAR} leaves no file at all — this is the observation that turns
  # "the source greps right" into "the binary honoured it".
  if [[ ! -f "${TRACE_FILE}" ]]; then
    err "no trace file at ${TRACE_FILE} — the relay did not honour ${TRACE_ENV_VAR}"
    err "  (the source probe passed, so the binary under test is not the source: rebuild)"
    PHASE_B_FAILURES+=("no-trace-file")
    return 1
  fi
  ok "relay wrote $(wc -l < "${TRACE_FILE}") trace step(s) to ${TRACE_FILE}"

  return 0
}

# phase_b_replay — hand the captured trace to the checker. Kept separate from
# the driving so an empty-but-present trace still reaches the checker and fails
# as the checker's own coverage breach, rather than being pre-judged here.
phase_b_replay() {
  runx "Build the replay entrypoint (check-trace)" \
    -- cargo build --profile "${PROFILE}" -p buzz-conformance --bin check-trace \
    || { PHASE_B_FAILURES+=("checker-build"); return 1; }

  CHECKER_BIN="${REPO_ROOT}/target/$(profile_target_dir "${PROFILE}")/check-trace"

  step "Replay: check-trace --group-by ${GATES_CONFORMANCE_GROUP_BY} --require ${GATES_CONFORMANCE_REQUIRE}"
  preview "${CHECKER_BIN}" --json --group-by "${GATES_CONFORMANCE_GROUP_BY}" \
    --require "${GATES_CONFORMANCE_REQUIRE}" "${TRACE_FILE}"
  note "exit 0 conform / 1 non-conformant / 2 could-not-read — 1 and 2 are never conflated"
  is_dry && return 0

  [[ -x "${CHECKER_BIN}" ]] || { err "checker binary missing: ${CHECKER_BIN}"; PHASE_B_FAILURES+=("checker-missing"); return 1; }

  set +e
  "${CHECKER_BIN}" --group-by "${GATES_CONFORMANCE_GROUP_BY}" \
    --require "${GATES_CONFORMANCE_REQUIRE}" "${TRACE_FILE}" > "${CHECK_TXT}" 2>&1
  local human_rc=$?
  "${CHECKER_BIN}" --json --group-by "${GATES_CONFORMANCE_GROUP_BY}" \
    --require "${GATES_CONFORMANCE_REQUIRE}" "${TRACE_FILE}" > "${CHECK_JSON}" 2>/dev/null
  local json_rc=$?
  set -e

  cat "${CHECK_TXT}"

  case "${human_rc}" in
    0) ok "check-trace: CONFORM" ;;
    1) err "check-trace: NON-CONFORMANT"; PHASE_B_FAILURES+=("non-conformant") ;;
    *) err "check-trace could not form an opinion (exit ${human_rc}) — this is NOT a conformance verdict"
       PHASE_B_FAILURES+=("checker-unusable")
       PHASE_B_NOTE="check-trace exited ${human_rc}: it could not read the trace, so no verdict was reached" ;;
  esac
  [[ ${json_rc} -eq ${human_rc} ]] || warn "check-trace --json disagreed on exit status (${json_rc} vs ${human_rc})"

  [[ ${#PHASE_B_FAILURES[@]} -eq 0 ]]
}

step "PHASE B — live trace replay against the candidate relay"
note "harness up -> candidate relay with ${TRACE_ENV_VAR} -> workload -> clean stop -> check-trace"

if [[ "${SKIP_PHASE_B}" == "1" ]]; then
  PHASE_B=skipped
  warn "--skip-phase-b: the RELAY is not under test this run; phase A only."
elif ! probe_emission_wiring; then
  # Never silently green: the gate's whole claim in phase B is "this relay
  # emitted a conforming trace", and a tree missing the wiring cannot make it.
  PHASE_B=fail
  banner "${RED}" \
    "CONFORMANCE PHASE B CANNOT RUN — WIRING MISSING" \
    "" \
    "${WIRING_MISSING[@]}" \
    "" \
    "Phase B is not optional cosmetics: without it gate 2 proves the CHECKER" \
    "and says nothing about this candidate relay. Failing rather than" \
    "downgrading to 'phase A passed' is deliberate."
  printf '%s\n' "${WIRING_MISSING[@]}" | tee "${EVIDENCE}/phase-b-wiring-missing.txt" >/dev/null
  for m in "${WIRING_MISSING[@]}"; do err "  ${m}"; done
else
  ok "wiring present: relay honours ${TRACE_ENV_VAR}; check-trace entrypoint exists"
  harness_guard || exit 2
  harness_arm_teardown
  if harness_up; then
    if phase_b_drive && phase_b_replay; then
      PHASE_B=pass
    else
      PHASE_B=fail
    fi
  else
    # Environmental, not a verdict: docker/compose could not give us a stack.
    # Recorded as `blocked` so nobody reads it as evidence either way.
    PHASE_B=blocked
    PHASE_B_NOTE="isolated harness (compose project ${GATES_PROJECT}) failed to come up; no relay was ever started"
    PHASE_B_FAILURES+=("harness-up")
    err "Phase B BLOCKED — ${PHASE_B_NOTE}"
  fi
  # Advisory only: an ERROR line in the relay log is worth seeing, but the
  # conformance claim is about the trace the relay emitted, and failing on
  # unrelated log noise would make this gate's verdict mean something else.
  if ! is_dry && [[ -f "${EVIDENCE}/relay.log" ]]; then
    assert_no_error_lines "${EVIDENCE}/relay.log" || warn "relay ERROR lines are advisory for this gate (recorded in details)"
  fi
fi

# ---- Verdict ----------------------------------------------------------------
# `pass` here now means BOTH halves held: the checker still bites (A) and this
# candidate relay emitted a trace it accepts (B). That is a strictly stronger
# claim than this gate made while phase B was blocked, and the stamp's
# details.phase_b carries the trace's own hash so the claim is re-checkable.

if is_dry; then
  record_result "${EVIDENCE}" conformance dry-run "${STARTED}" \
    "$(jq -n --arg require "${GATES_CONFORMANCE_REQUIRE}" --arg group "${GATES_CONFORMANCE_GROUP_BY}" \
      '{note:"planned only",
        phase_a:"cargo test -p buzz-conformance --all-targets",
        phase_b:{plan:"harness -> candidate relay with BUZZ_CONFORMANCE_TRACE_PATH -> workload -> check-trace",
                 required_critical_actions:($require|split(",")), group_by:$group}}')"
  print_result_line conformance dry-run 0 "${EVIDENCE}"
  exit 0
fi

RESULT=pass
[[ "${PHASE_A}" == "pass" ]] || RESULT=fail
case "${PHASE_B}" in
  pass|skipped) ;;
  blocked)      RESULT=blocked ;;
  *)            RESULT=fail ;;
esac

REPLAY_JSON='null'
[[ -s "${CHECK_JSON}" ]] && REPLAY_JSON="$(cat "${CHECK_JSON}")"

RELAY_ERROR_LINES=0
if [[ -f "${EVIDENCE}/relay.log" ]]; then
  RELAY_ERROR_LINES="$(grep -cE '(^|[[:space:]])(ERROR|level=error)([[:space:]]|$)' "${EVIDENCE}/relay.log" 2>/dev/null || true)"
  RELAY_ERROR_LINES="${RELAY_ERROR_LINES:-0}"
fi

DETAILS="$(jq -n \
  --arg phase_a "${PHASE_A}" \
  --arg phase_b "${PHASE_B}" \
  --arg note "${PHASE_B_NOTE}" \
  --arg trace_path "${TRACE_FILE}" \
  --arg trace_sha "$(sha256_file "${TRACE_FILE}")" \
  --argjson trace_bytes "$(file_bytes "${TRACE_FILE}")" \
  --arg require "${GATES_CONFORMANCE_REQUIRE}" \
  --arg group_by "${GATES_CONFORMANCE_GROUP_BY}" \
  --argjson replay "${REPLAY_JSON}" \
  --argjson relay_error_lines "${RELAY_ERROR_LINES}" \
  --argjson failures "$(printf '%s\n' "${PHASE_B_FAILURES[@]:-}" | jq -R . | jq -s 'map(select(length>0))')" \
  '{phase_a:{name:"checker integrity (cargo test -p buzz-conformance --all-targets)", result:$phase_a},
    phase_b:{name:"live trace replay against candidate relay", result:$phase_b,
             note:(if ($note|length)>0 then $note else null end),
             trace:{path:$trace_path, sha256:$trace_sha, bytes:$trace_bytes},
             required_critical_actions:($require|split(",")),
             group_by:$group_by,
             replay:$replay,
             relay_error_lines:$relay_error_lines,
             failures:$failures},
    proves:"the independent replay checker still rejects illegal transitions, foreign-row leaks and coverage breaches (A), AND the candidate relay, driven through the shared write/read workload, emitted a JSONL trace that checker accepts (B)",
    does_not_prove:"conformance beyond the executions actually run — one repo announce, one issue, one history read across two actors. Not a proof; no ACP agent lifecycle (that is gates 3/4); and under group_by=state the state_mismatch mode is carried by phase A fixtures, not by the live trace"}')"

record_result "${EVIDENCE}" conformance "${RESULT}" "${STARTED}" "${DETAILS}"
DURATION=$(( $(epoch_s) - STARTED ))
print_result_line conformance "${RESULT}" "${DURATION}" "${EVIDENCE}"
[[ "${RESULT}" == "pass" ]]
