# shellcheck shell=bash
# =============================================================================
# lib/harness.sh — the ISOLATED stack the gates run against.
# =============================================================================
# This is scripts/start-isolated-test-relay.sh's pattern, re-cut as a library so
# gate scripts can bring the stack up, assert against it, and — the part that
# matters — guarantee it goes away again.
#
# Differences from scripts/start-isolated-test-relay.sh, all deliberate:
#
#   1. Compose project defaults to `buzz-gates`, NOT `buzz-harness`. On this box
#      `buzz-harness` is routinely already up for a sibling worktree; a
#      promotion run that tore it down would be sabotage. `harness_guard`
#      hard-refuses the known-shared project names.
#   2. Ports are shifted (5473/6473/9473/9474/3031/8089/9203) via
#      docker-compose.gates.yml so both stacks coexist.
#   3. The relay is daemonised with `setsid`, not tmux. tmux is NOT installed on
#      this host; setsid solves the same problem the tmux comment in
#      start-isolated-test-relay.sh:130-133 describes (the ephemeral invoking
#      shell's process group gets reaped and SIGTERMs a foreground relay).
#   4. Teardown is a trap, not a documented follow-up command. See
#      `harness_arm_teardown`.
#
# Requires lib/common.sh.
# =============================================================================

# ---- topology ---------------------------------------------------------------

GATES_PROJECT="${GATES_PROJECT:-buzz-gates}"
export GATES_PG_PORT="${GATES_PG_PORT:-5473}"
export GATES_REDIS_PORT="${GATES_REDIS_PORT:-6473}"
export GATES_MINIO_PORT="${GATES_MINIO_PORT:-9473}"
export GATES_MINIO_CONSOLE_PORT="${GATES_MINIO_CONSOLE_PORT:-9474}"
GATES_RELAY_PORT="${GATES_RELAY_PORT:-3031}"
GATES_RELAY_HEALTH_PORT="${GATES_RELAY_HEALTH_PORT:-8089}"
GATES_RELAY_METRICS_PORT="${GATES_RELAY_METRICS_PORT:-9203}"
GATES_POST_SCHEMA_SCRIPT_REL="scripts/reconcile-schema-after-pgschema.sql"

# Compose projects this runner must NEVER drive. buzz-harness belongs to a
# sibling worktree's test session; buzz-prod / buzz are live. Refusing by name
# is crude but it is the failure mode that actually happens (a stray
# --project-name typo), and the blast radius of getting it wrong is a
# production outage.
GATES_FORBIDDEN_PROJECTS=(buzz-prod buzz buzz-harness evaperf)

harness_guard() {
  local p
  for p in "${GATES_FORBIDDEN_PROJECTS[@]}"; do
    if [[ "${GATES_PROJECT}" == "${p}" ]]; then
      err "REFUSING to use compose project '${GATES_PROJECT}' — it is shared or live."
      err "The gate harness must own its project exclusively. Pass --project-name buzz-gates."
      return 1
    fi
  done
  return 0
}

harness_compose() {
  docker compose -p "${GATES_PROJECT}" \
    -f "${REPO_ROOT}/docker-compose.harness.yml" \
    -f "${REPO_ROOT}/scripts/selfhost/gates/docker-compose.gates.yml" "$@"
}

harness_relay_url()  { echo "http://localhost:${GATES_RELAY_PORT}"; }
harness_relay_ws()   { echo "ws://localhost:${GATES_RELAY_PORT}"; }
harness_host_label() { echo "localhost:${GATES_RELAY_PORT}"; }

# ---- teardown ---------------------------------------------------------------

# harness_arm_teardown — install the EXIT trap. Call this BEFORE the first
# `harness_up`. Teardown is unconditional: a gate that fails halfway must still
# leave zero containers behind, or the next run inherits poisoned state and the
# box slowly fills with orphaned stacks.
harness_arm_teardown() {
  GATES_TEARDOWN_ARMED=1
  trap 'harness_down' EXIT INT TERM
}

# harness_down — idempotent full teardown. Safe to call when nothing is up.
# Honours --no-teardown (GATES_NO_TEARDOWN=1) for debugging, but says so loudly
# because a left-up stack is a footgun for the next run.
harness_down() {
  local rc=$?
  trap - EXIT INT TERM
  [[ "${GATES_TEARDOWN_ARMED:-0}" == "1" ]] || return "${rc}"

  harness_relay_stop
  harness_acp_stop_all

  if [[ "${GATES_NO_TEARDOWN:-0}" == "1" ]]; then
    warn "--no-teardown set: leaving compose project '${GATES_PROJECT}' UP."
    warn "Tear it down yourself: docker compose -p ${GATES_PROJECT} -f docker-compose.harness.yml -f scripts/selfhost/gates/docker-compose.gates.yml down -v --remove-orphans"
    return "${rc}"
  fi

  if is_dry; then return "${rc}"; fi

  log "Tearing down compose project '${GATES_PROJECT}' (volumes included)..."
  harness_compose down -v --remove-orphans >>"${GATES_HARNESS_LOG:-/dev/null}" 2>&1 || true
  harness_assert_torn_down || true
  return "${rc}"
}

# harness_assert_torn_down — the teardown GUARANTEE, checked rather than
# assumed. Returns non-zero (and shouts) if any container still carries our
# compose project label.
harness_assert_torn_down() {
  local left
  left="$(docker ps -a --filter "label=com.docker.compose.project=${GATES_PROJECT}" \
            --format '{{.Names}}' 2>/dev/null | tr '\n' ' ')"
  if [[ -n "${left// /}" ]]; then
    err "Teardown incomplete — containers remain for project '${GATES_PROJECT}': ${left}"
    return 1
  fi
  ok "Teardown verified: zero containers for project '${GATES_PROJECT}'."
  return 0
}

# ---- bring-up ---------------------------------------------------------------

harness_up() {
  harness_guard || return 1
  runx "Bring up isolated backing services (project=${GATES_PROJECT}, pg=${GATES_PG_PORT} redis=${GATES_REDIS_PORT} minio=${GATES_MINIO_PORT})" \
    -- docker compose -p "${GATES_PROJECT}" \
       -f "${REPO_ROOT}/docker-compose.harness.yml" \
       -f "${REPO_ROOT}/scripts/selfhost/gates/docker-compose.gates.yml" up -d || return 1

  step "Wait for Postgres to accept connections (<=120s)"
  note "docker compose -p ${GATES_PROJECT} exec -T postgres pg_isready -U buzz, polled"
  is_dry && return 0

  local i
  for i in $(seq 1 60); do
    if harness_compose exec -T postgres pg_isready -U buzz >/dev/null 2>&1; then
      ok "Postgres ready on :${GATES_PG_PORT}"
      return 0
    fi
    sleep 2
  done
  err "Postgres did not become ready"
  return 1
}

# harness_schema — reset + apply migrations and canonical post-schema
# convergence. The database belongs
# solely to our compose project, so a destructive reset every run is correct:
# stale partitions from an earlier gate run must not colour this verdict.
#
# Schema comes from the migrations files, applied in filename order — NOT from
# schema/schema.sql. Production relays build their schema by running exactly
# these files (BUZZ_AUTO_MIGRATE=true), and the first live gate runs proved the
# declarative schema has drifted from them: git_repo_names (0002),
# parameterized_event_watermarks (0007) and product_feedback (0017) are all
# absent from schema.sql, so a gate that trusted it judged candidates against
# a database no production relay has ever run on. The drift itself is an
# upstream bug worth fixing; until it is, the migrations are the only schema
# source that provably matches prod. Applied ledger-less via psql, so the gate
# relays keep auto-migrate OFF — a second applier would collide on migration 1.
harness_schema() {
  step "Reset isolated database and apply migrations + canonical post-schema convergence"
  preview "psql < migrations/*.sql (in order), then ${GATES_POST_SCHEMA_SCRIPT_REL}"
  is_dry && return 0

  harness_compose exec -T postgres psql -U buzz -d buzz -v ON_ERROR_STOP=1 \
    -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;" >/dev/null || return 1

  # -1 wraps each file in a transaction, matching how sqlx applies these same
  # files in production — 0007 opens with LOCK TABLE, which is only legal
  # inside a transaction block, and others may rely on all-or-nothing apply.
  local m
  for m in "${REPO_ROOT}"/migrations/*.sql; do
    harness_compose exec -T postgres psql -U buzz -d buzz -v ON_ERROR_STOP=1 -1 \
      < "${m}" >/dev/null || { err "migration failed: $(basename "${m}")"; return 1; }
  done

  harness_compose exec -T postgres psql -U buzz -d buzz -v ON_ERROR_STOP=1 \
    < "${REPO_ROOT}/${GATES_POST_SCHEMA_SCRIPT_REL}" >/dev/null || return 1

  ok "Schema applied from $(ls "${REPO_ROOT}"/migrations/*.sql | wc -l) migrations plus canonical post-schema convergence"
}

# harness_seed — community/channels/members, keyed to OUR relay's host label so
# the tenant binding matches. setup-desktop-test-data.sh is the repo's single
# writer of this seed; we drive it with overridden DB + host env exactly as
# start-isolated-test-relay.sh:111-115 does.
harness_seed() {
  step "Seed community (host=$(harness_host_label)), channels, members"
  preview ./scripts/setup-desktop-test-data.sh
  is_dry && return 0

  BUZZ_COMMUNITY_HOST="$(harness_host_label)" \
  BUZZ_DB_HOST=localhost BUZZ_DB_PORT="${GATES_PG_PORT}" BUZZ_DB_USER=buzz \
  BUZZ_DB_PASS=buzz_dev BUZZ_DB_NAME=buzz \
  BUZZ_DB_DOCKER_CONTAINER="${GATES_PROJECT}-postgres-1" \
    "${REPO_ROOT}/scripts/setup-desktop-test-data.sh" >>"${GATES_HARNESS_LOG:-/dev/null}" 2>&1 || return 1
  ok "Community seeded"
}

# ---- relay ------------------------------------------------------------------

# harness_relay_build <profile> — build the CANDIDATE relay from source on the
# current branch. Note buzz-relay's TEST targets do not build on this host
# (no openssl dev headers, via dev-deps) but the lib and bins do; see
# gates/README.md "Known environment gap".
harness_relay_build() {
  local profile="$1"
  # If run-gates.sh already staged an immutable candidate, building again would
  # be pointless work AND actively harmful: the rebuild rewrites
  # target/<profile>/buzz-relay, and if anything later hashed target/ instead of
  # the staged copy the binding would flap. The staged copy is the candidate.
  if [[ -n "${GATES_ARTIFACT_DIR:-}" && -x "${GATES_ARTIFACT_DIR}/buzz-relay" ]]; then
    step "Use staged candidate relay (already built and hash-locked)"
    note "${GATES_ARTIFACT_DIR}/buzz-relay"
    return 0
  fi
  ensure_cargo || return 1
  runx "Build candidate relay from source (profile=${profile})" \
    -- cargo build --profile "${profile}" -p buzz-relay
}

# harness_relay_start <binary> <logfile> [extra env KEY=VAL ...]
harness_relay_start() {
  local bin="$1" logfile="$2"; shift 2
  local extra=("$@")

  step "Start relay ${bin} on :${GATES_RELAY_PORT} (health :${GATES_RELAY_HEALTH_PORT}, metrics :${GATES_RELAY_METRICS_PORT})"
  preview "${bin}"
  note "daemonised with setsid (tmux is not installed on this host); log -> ${logfile}"
  [[ ${#extra[@]} -gt 0 ]] && note "extra env: ${extra[*]}"
  is_dry && return 0

  if ss -ltn 2>/dev/null | grep -q ":${GATES_RELAY_PORT} "; then
    err "Port ${GATES_RELAY_PORT} already in use; refusing to mistake a stale relay for this harness."
    return 1
  fi
  [[ -x "${bin}" ]] || { err "relay binary not executable: ${bin}"; return 1; }

  setsid env \
    DATABASE_URL="postgres://buzz:buzz_dev@localhost:${GATES_PG_PORT}/buzz" \
    REDIS_URL="redis://localhost:${GATES_REDIS_PORT}" \
    RELAY_URL="$(harness_relay_ws)" \
    BUZZ_BIND_ADDR="0.0.0.0:${GATES_RELAY_PORT}" \
    BUZZ_HEALTH_PORT="${GATES_RELAY_HEALTH_PORT}" \
    BUZZ_METRICS_PORT="${GATES_RELAY_METRICS_PORT}" \
    BUZZ_S3_ENDPOINT="http://localhost:${GATES_MINIO_PORT}" \
    BUZZ_S3_ACCESS_KEY=buzz_dev \
    BUZZ_S3_SECRET_KEY=buzz_dev_secret \
    BUZZ_S3_BUCKET=buzz-media \
    BUZZ_REQUIRE_AUTH_TOKEN=false \
    BUZZ_RECONCILE_CHANNELS=true \
    "${extra[@]}" \
    "${bin}" >"${logfile}" 2>&1 < /dev/null &
  GATES_RELAY_PID=$!
  disown "${GATES_RELAY_PID}" 2>/dev/null || true

  local i
  for i in $(seq 1 60); do
    if curl -sf -o /dev/null "$(harness_relay_url)/" 2>/dev/null; then
      ok "Relay live at $(harness_relay_url) (pid ${GATES_RELAY_PID})"
      return 0
    fi
    kill -0 "${GATES_RELAY_PID}" 2>/dev/null || { err "Relay exited during startup — see ${logfile}"; tail -30 "${logfile}" >&2; return 1; }
    sleep 1
  done
  err "Relay did not come up on :${GATES_RELAY_PORT} within 60s — see ${logfile}"
  return 1
}

harness_relay_stop() {
  [[ -n "${GATES_RELAY_PID:-}" ]] || return 0
  kill "${GATES_RELAY_PID}" 2>/dev/null || true
  local i
  for i in $(seq 1 20); do
    kill -0 "${GATES_RELAY_PID}" 2>/dev/null || { GATES_RELAY_PID=""; return 0; }
    sleep 0.5
  done
  kill -9 "${GATES_RELAY_PID}" 2>/dev/null || true
  GATES_RELAY_PID=""
}

# ---- acp process bookkeeping ------------------------------------------------
# Skew/soak gates start acp binaries; every pid is registered here so teardown
# can reap them even when a gate dies mid-assertion.

GATES_ACP_PIDS=()

harness_acp_register() { GATES_ACP_PIDS+=("$1"); }

harness_acp_stop_all() {
  local pid
  for pid in "${GATES_ACP_PIDS[@]:-}"; do
    [[ -n "${pid}" ]] || continue
    kill "${pid}" 2>/dev/null || true
  done
  sleep 1
  for pid in "${GATES_ACP_PIDS[@]:-}"; do
    [[ -n "${pid}" ]] || continue
    kill -9 "${pid}" 2>/dev/null || true
  done
  GATES_ACP_PIDS=()
}

# ---- log marker assertions --------------------------------------------------
# Gate verdicts come from these, never from a human reading a log.

# wait_for_marker <logfile> <marker> <timeout_s> — 0 if seen, 1 on timeout.
wait_for_marker() {
  local logfile="$1" marker="$2" timeout="${3:-60}" waited=0
  while (( waited < timeout )); do
    if [[ -f "${logfile}" ]] && grep -qF -- "${marker}" "${logfile}"; then return 0; fi
    sleep 1
    waited=$(( waited + 1 ))
  done
  return 1
}

assert_marker() {
  local logfile="$1" marker="$2"
  if grep -qF -- "${marker}" "${logfile}" 2>/dev/null; then
    ok "marker present: ${marker}"
    return 0
  fi
  err "marker ABSENT: ${marker}  (log: ${logfile})"
  return 1
}

# assert_no_error_lines <logfile> — fails if any ERROR-level line appears.
# Deliberately strict: a gate that tolerates ERROR lines is a gate that will
# one day wave through a broken build that "mostly worked".
assert_no_error_lines() {
  local logfile="$1" hits
  hits="$(grep -cE '(^|[[:space:]])(ERROR|level=error)([[:space:]]|$)' "${logfile}" 2>/dev/null || true)"
  hits="${hits:-0}"
  if [[ "${hits}" -gt 0 ]]; then
    err "${hits} ERROR line(s) in ${logfile}:"
    grep -nE '(^|[[:space:]])(ERROR|level=error)([[:space:]]|$)' "${logfile}" | head -10 >&2
    return 1
  fi
  ok "no ERROR lines in $(basename "${logfile}")"
  return 0
}

# assert_dropped_zero <logfile> — every `dropped=N` / `dropped_total=N` counter
# the process logged must be 0. buzz-acp logs these when a publisher lags
# (crates/buzz-acp/src/lib.rs:887, :985) or when gated frames are shed
# (crates/buzz-acp/src/relay.rs:1611, :1645). Non-zero means the run dropped
# work, which makes any downstream "it answered the comment" assertion hollow.
assert_dropped_zero() {
  local logfile="$1" bad
  bad="$(grep -oE '\bdropped(_total)?[= ]+[0-9]+' "${logfile}" 2>/dev/null \
         | grep -vE '[= ]+0$' || true)"
  if [[ -n "${bad}" ]]; then
    err "non-zero dropped counters in ${logfile}:"
    echo "${bad}" | head -10 >&2
    return 1
  fi
  ok "dropped=0 (no lagged/shed counters) in $(basename "${logfile}")"
  return 0
}
