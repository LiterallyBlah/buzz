# shellcheck shell=bash
# =============================================================================
# lib/workload.sh — the synthetic workload the skew and soak gates drive.
# =============================================================================
# One scripted scenario, reused by gate-skew.sh (once per pairing) and
# gate-soak.sh (looped). Keeping it in one place is the point: if soak and skew
# exercised different paths, a soak pass would say nothing about the thing skew
# proved.
#
# THE SCENARIO (project-routing lane, the one that produces the enrolment
# markers the release plan names):
#
#   1. Mint a throwaway keypair for the agent and one for the driver. Keys are
#      per-run and never persisted — nothing here touches /opt/buzz/keys.
#   2. Start buzz-acp against the harness relay with project routing on and
#      BUZZ_ACP_AGENT_COMMAND pointed at gates/acp-stub-agent.mjs, so the turn
#      is deterministic and no LLM is involved.
#   3. Announce a repo (kind:30617) from the agent's own key so discovery has
#      something to find.
#   4. Open an issue (kind:1621) whose body EXPLICITLY @mentions the agent's
#      display name and whose p-tag names the agent. Both are required: an
#      unknown root only enrols on Addressing::ExplicitMention
#      (crates/buzz-acp/src/project.rs:7892-7913 `wake_or_enrol`; token matcher
#      project.rs:7134-7162). A structural p-tag alone is deliberately ignored
#      by the harness, so a workload that only p-tagged would silently prove
#      nothing.
#   5. Wait for the agent's reply on that root.
#
# Requires lib/common.sh and lib/harness.sh.
# =============================================================================

GATES_STUB_AGENT="${GATES_STUB_AGENT:-${GATES_DIR}/acp-stub-agent.mjs}"
GATES_AGENT_DISPLAY_NAME="${GATES_AGENT_DISPLAY_NAME:-GatesProbe}"

# workload_generate_key -> "<seckey_hex>\t<pubkey_hex>"
# buzz-admin generate-key is the repo's key minter (crates/buzz-admin/src/main.rs:132-137);
# it needs no DB for this subcommand. buzz-admin carries no openssl dependency,
# so it builds on this host despite the relay's test-target gap.
workload_generate_key() {
  local out
  out="$(cargo run --quiet -p buzz-admin -- generate-key 2>/dev/null)" || return 1
  local pub sec
  pub="$(echo "${out}" | awk '/Public key:/ {print $NF}')"
  sec="$(echo "${out}" | awk '/Secret key:/ {print $NF}')"
  [[ -n "${pub}" && -n "${sec}" ]] || return 1
  printf '%s\t%s\n' "${sec}" "${pub}"
}

# workload_start_acp <acp_binary> <logfile> <seckey> <state_dir>
# Registers the pid with the harness so teardown reaps it even on a mid-gate
# abort. Returns after the connect marker appears (or fails on timeout).
workload_start_acp() {
  local acp_bin="$1" logfile="$2" seckey="$3" state_dir="$4"
  mkdir -p "${state_dir}"

  [[ -x "${acp_bin}" ]] || { err "acp binary not executable: ${acp_bin}"; return 1; }

  setsid env \
    BUZZ_PRIVATE_KEY="${seckey}" \
    BUZZ_RELAY_URL="$(harness_relay_ws)" \
    BUZZ_ACP_AGENT_COMMAND="node" \
    BUZZ_ACP_AGENT_ARGS="${GATES_STUB_AGENT}" \
    BUZZ_ACP_DISPLAY_NAME="${GATES_AGENT_DISPLAY_NAME}" \
    BUZZ_ACP_PROJECT_ROUTING_ENABLED=true \
    BUZZ_ACP_RESPOND_TO=anyone \
    BUZZ_ACP_SUBSCRIBE=mentions \
    BUZZ_ACP_STATE_DIR="${state_dir}" \
    BUZZ_ACP_AGENTS=1 \
    BUZZ_ACP_IDLE_TIMEOUT=120 \
    RUST_LOG="${GATES_ACP_LOG:-buzz_acp=info}" \
    "${acp_bin}" > "${logfile}" 2>&1 < /dev/null &
  local pid=$!
  harness_acp_register "${pid}"
  echo "${pid}"
}

# workload_announce_repo <cli_bin> <seckey> <repo_id>
workload_announce_repo() {
  local cli="$1" seckey="$2" repo_id="$3"
  BUZZ_PRIVATE_KEY="${seckey}" BUZZ_RELAY_URL="$(harness_relay_ws)" \
    "${cli}" repos create --id "${repo_id}" \
      --name "buzz-gates promotion probe" \
      --description "throwaway repo announced by the staging gate runner" 2>&1
}

# workload_open_issue <cli_bin> <driver_seckey> <repo_owner_hex> <repo_id>
# Emits the created issue's event id on stdout (the "root").
workload_open_issue() {
  local cli="$1" seckey="$2" owner="$3" repo_id="$4" out
  out="$(BUZZ_PRIVATE_KEY="${seckey}" BUZZ_RELAY_URL="$(harness_relay_ws)" \
    "${cli}" issues create \
      --repo-owner "${owner}" \
      --repo-id "${repo_id}" \
      --title "promotion gate probe" \
      --content "@${GATES_AGENT_DISPLAY_NAME} please acknowledge this promotion probe" \
      --to "${owner}" 2>&1)" || { echo "${out}" >&2; return 1; }
  # The CLI prints the event id; take the first 64-hex token it emitted.
  echo "${out}" | grep -oE '\b[0-9a-f]{64}\b' | head -1
}

# workload_wait_reply <cli_bin> <driver_seckey> <root_event_id> <timeout_s>
# The agent's reply is a comment on the root. Polling the root's history is the
# read-side counterpart to the write we just made.
workload_wait_reply() {
  local cli="$1" seckey="$2" root="$3" timeout="${4:-90}" waited=0 out
  while (( waited < timeout )); do
    out="$(BUZZ_PRIVATE_KEY="${seckey}" BUZZ_RELAY_URL="$(harness_relay_ws)" \
      "${cli}" projects history --root "${root}" 2>/dev/null || true)"
    if echo "${out}" | grep -qF "buzz-gates stub agent"; then
      return 0
    fi
    sleep 3
    waited=$(( waited + 3 ))
  done
  return 1
}

# workload_shutdown_acp <pid> <logfile> <timeout_s>
# SIGTERM, then wait for the harness's own final line. buzz-acp handles SIGTERM
# (crates/buzz-acp/src/lib.rs:2278-2282) and logs "buzz-acp stopped" (lib.rs:3641)
# as its last act — asserting on that is how we distinguish a clean shutdown
# from a process that merely died.
workload_shutdown_acp() {
  local pid="$1" logfile="$2" timeout="${3:-45}"
  kill -TERM "${pid}" 2>/dev/null || true
  wait_for_marker "${logfile}" "buzz-acp stopped" "${timeout}"
}
