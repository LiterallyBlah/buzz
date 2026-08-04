#!/usr/bin/env bash
# =============================================================================
# deploy.sh — the self-hosted Buzz release executor.
# =============================================================================
#
# This script is the ONLY thing that swaps binaries and images on the live box.
# That is the whole design: Buzz's agents develop Buzz inside the repo, and the
# one component that mutates live state sits outside the blast radius of what
# they are changing. An agent proposes a release by minting a manifest; this
# executor decides whether to believe it.
#
# It is a transcription of the deploy that was performed by hand on 2026-08-04
# and verified end to end. Nothing here is speculative: every gate below is a
# check that was actually run, in the order it was actually run.
#
# THE FIVE RULES THIS SCRIPT EXISTS TO ENFORCE
#
#   1. DRY RUN IS THE DEFAULT. `deploy.sh manifest.json` reports; only
#      `--execute` touches anything. Deploying is a deliberate spelling because
#      the failure mode of the opposite default is unrecoverable and the
#      failure mode of this default is a wasted minute.
#   2. RELAY FIRST, PROVEN HEALTHY, THEN AGENTS — never in one step. The relay
#      is the leg every agent stands on. Move one leg at a time and something
#      is always holding you up; move both and a bad build takes the box down
#      with no working component left to tell you about it.
#   3. NOTHING IS FETCHED. Every artifact must already exist locally, and its
#      identity must match the manifest byte for byte. A deployer that can
#      reach out and get what it is missing is a deployer that can be told to
#      get something else.
#   4. BACK UP BEFORE, VERIFY AFTER. Every destructive step announces its
#      intent before acting and re-reads the result after, so a journal shows
#      what was attempted even if the machine died mid-step.
#   5. FAILURE ROLLS BACK. Any gate failure restores the previous binaries and
#      the previous image, restarts, re-gates, and exits nonzero with a report.
#      Rollback undoes exactly what this run did, and nothing else.
#
# OUTPUT CONTRACT
#
#   Every line is `buzz-deploy ts=<iso8601> step=<name> status=<STATUS> <detail>`.
#   STATUS is one of PASS FAIL SKIP PLAN INFO WARN. A systemd unit runs this
#   and humans and agents read the journal, so:
#       journalctl -u buzz-deployer -o cat | grep 'status=FAIL'
#   is the entire triage procedure. Steps are named `<phase>.<thing>` and the
#   names are stable; treat them as an interface.
#
# EXIT CODES
#
#   0  success (or a dry run with no blockers)
#   1  preflight refused, or a dry run found blockers — nothing was mutated
#   2  usage error or invalid manifest — nothing was mutated
#   3  deploy failed after mutation and rolled back cleanly
#   4  deploy failed AND ROLLBACK FAILED — a human must intervene now
#
# USAGE
#
#   scripts/selfhost/deploy.sh [options] <manifest.json>
#   scripts/selfhost/deploy.sh [options] --inbox /opt/buzz/releases
#
# Options are documented in usage() below. Everything the script touches is
# overridable by environment variable so that a dry run can be pointed at a
# scratch tree and exercise the real code path rather than a rehearsal of it.
# =============================================================================

set -Eeuo pipefail

# -----------------------------------------------------------------------------
# Deployment topology.
#
# These defaults describe the live box exactly. They are variables and not
# literals for one reason: a dry run against a scratch BUZZ_ROOT must execute
# the same lines of code as the real thing. A test that runs different code is
# not a test of anything.
# -----------------------------------------------------------------------------
BUZZ_ROOT="${BUZZ_ROOT:-/opt/buzz}"
RELAY_DIR="${BUZZ_RELAY_DIR:-${BUZZ_ROOT}/relay}"
RELAY_ENV="${RELAY_DIR}/.env"
RELAY_RUN="${RELAY_DIR}/run.sh"
BIN_DIR="${BUZZ_BIN_DIR:-${BUZZ_ROOT}/bin}"
BACKUP_ROOT="${BUZZ_BACKUP_ROOT:-${BUZZ_ROOT}/backups}"
FULL_BACKUP_SCRIPT="${BUZZ_FULL_BACKUP:-${BUZZ_ROOT}/scripts/backup-buzz-latest.py}"
BUZZ_CLI="${BUZZ_CLI:-${BIN_DIR}/buzz}"
REPO="${BUZZ_DEPLOY_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
MINTER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/mint-manifest.py"

RELAY_UNIT="${BUZZ_RELAY_UNIT:-buzz-relay.service}"
# Agent units are restarted one at a time, in this order — see gate_agent().
read -r -a AGENT_UNITS <<<"${BUZZ_AGENT_UNITS:-buzz-claude.service buzz-codex.service}"
COMPOSE_PROJECT="${BUZZ_COMPOSE_PROJECT:-buzz-prod}"
LIVENESS_LOCAL="${BUZZ_LIVENESS_LOCAL:-http://127.0.0.1:3100/_liveness}"
LIVENESS_PUBLIC="${BUZZ_LIVENESS_PUBLIC:-https://hermes.tail81f3.ts.net:9443/_liveness}"

# Gate budgets. The 60s agent window is the one the manual runbook used: it is
# long enough for enrolment history reconstruction to finish on a cold relay and
# short enough that a wedged agent is caught before the next one is touched.
RELAY_GATE_SECONDS="${BUZZ_DEPLOY_RELAY_GATE_SECONDS:-120}"
AGENT_GATE_SECONDS="${BUZZ_DEPLOY_AGENT_GATE_SECONDS:-60}"
FULL_BACKUP_MAX_AGE_MIN="${BUZZ_DEPLOY_FULL_BACKUP_MAX_AGE_MIN:-60}"
MIN_FREE_MIB="${BUZZ_DEPLOY_MIN_FREE_MIB:-2048}"

# The lines buzz-acp must emit within the gate window. Verified against
# crates/buzz-acp/src/lib.rs and crates/buzz-acp/src/relay.rs at 0fa54b3c:
# a startup banner, an initialised agent, the projects publisher, and a clean
# enrolment replay. Any one of these missing means the process came up but did
# not come up *working*, which is the failure a plain `systemctl is-active`
# cheerfully reports as success.
AGENT_REQUIRED_LINES=(
  "buzz-acp starting:"
  "agent initialized"
  "project activity publisher enabled"
  "enrolment history reconstruction complete"
)
# tracing's formatter pads the level to five columns after an RFC3339 timestamp
# that always ends in `Z`. Anchoring on that shape rather than on the bare word
# ERROR keeps a log message that merely mentions errors from failing the gate.
AGENT_ERROR_PATTERN="${BUZZ_DEPLOY_ERROR_PATTERN:-Z[[:space:]]+ERROR[[:space:]]}"

# -----------------------------------------------------------------------------
# Install policy.
#
# WHERE a component goes, WHO owns it and WHAT mode it gets are decided HERE,
# not in the manifest. This is the single most important security property of
# the whole design: this script runs as root, and if a manifest could name its
# own install path it could name /etc/systemd/system/anything.service and the
# release pipeline would become an arbitrary-root-write pipeline. The manifest
# says what an artifact *is*; this table says where it goes.
# -----------------------------------------------------------------------------
declare -A INSTALL_PATH=(
  [acp]="${BIN_DIR}/buzz-acp"
  [cli]="${BIN_DIR}/buzz"
)
declare -A INSTALL_OWNER=(
  # buzz-acp is root-owned so the hermes-run agents cannot rewrite the binary
  # they are about to execute. The CLI is hermes-owned because hermes runs it
  # interactively and root ownership would only add sudo to every invocation.
  [acp]="root:root"
  [cli]="hermes:hermes"
)
declare -A INSTALL_MODE=(
  [acp]="0755"
  [cli]="0755"
)

# -----------------------------------------------------------------------------
# Run state.
# -----------------------------------------------------------------------------
EXECUTE=false
MANIFEST=""
ARTIFACT_ROOT=""
INBOX_ROOT=""
RELEASE_DIR=""
ANNOUNCE_ROOT="${BUZZ_ANNOUNCE_ROOT:-}"
ACK_MIGRATIONS=false
RELAY_RESTART_MODE="${BUZZ_DEPLOY_RELAY_RESTART:-run.sh}"
declare -a ONLY_COMPONENTS=()

PASS_COUNT=0
FAIL_COUNT=0
BLOCKED=false            # a preflight said no
BACKUP_DIR=""
ROLLBACK_BIN=""
PREVIOUS_IMAGE=""
PREVIOUS_COMMIT=""
RESULT_CODE=0
RESULT_REASON="not started"
TRANSCRIPT="$(mktemp -t buzz-deploy.XXXXXX)"

# Mutation ledger. Rollback consults these rather than blindly restoring
# everything, because "undo what you did" and "put the box in some remembered
# state" are different operations and only the first one is safe to run
# unattended.
DID_SWAP_IMAGE=false
DID_INSTALL_ACP=false
DID_INSTALL_CLI=false
DID_RESTART_RELAY=false
declare -a RESTARTED_AGENTS=()

declare -A MF=()          # flattened manifest fields
declare -a ORDER=()       # deploy order, after --component narrowing

# -----------------------------------------------------------------------------
# Output.
# -----------------------------------------------------------------------------
now() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }

emit() {
  # Everything goes to stdout, including failures. Splitting FAIL onto stderr
  # would interleave unpredictably in the journal and destroy the one property
  # that makes this transcript useful: it reads top to bottom as what happened.
  local line
  printf -v line 'buzz-deploy ts=%s step=%s status=%s %s' "$(now)" "$1" "$2" "${3-}"
  printf '%s\n' "$line"
  printf '%s\n' "$line" >>"${TRANSCRIPT}"
}
step_pass() { PASS_COUNT=$((PASS_COUNT + 1)); emit "$1" PASS "${2-}"; }
step_fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); emit "$1" FAIL "${2-}"; }
step_skip() { emit "$1" SKIP "${2-}"; }
step_info() { emit "$1" INFO "${2-}"; }
step_warn() { emit "$1" WARN "${2-}"; }
step_plan() { emit "$1" PLAN "${2-}"; }

usage() {
  cat <<'MSG'
Usage: deploy.sh [options] <manifest.json>
       deploy.sh [options] --inbox <releases-root>

  --execute                 Actually deploy. Without it this is a dry run and
                            nothing is written, restarted or installed.
  --dry-run                 Explicit form of the default.
  --inbox <root>            Take the oldest complete release from
                            <root>/incoming/*/manifest.json, deploy it, then
                            move it to <root>/processed/ with a result.json.
                            Only --execute consumes a release; a dry run leaves
                            the inbox untouched.
  --artifact-root <dir>     Resolve manifest artifact paths against <dir>.
                            Default: the directory holding the manifest.
  --component <name>        Narrow the release to a subset of its components
                            (repeatable). Can only ever remove work, never add
                            it: a component not in the manifest is refused.
  --ack-migrations          Required when the manifest says migrations are
                            "ack-required". Deliberately a command-line act,
                            not a manifest field, so an unattended inbox drop
                            can never run a forward-only migration by itself.
  --announce-root <hex>     Post progress comments on this issue/PR root event.
                            Needs BUZZ_RELAY_URL, BUZZ_PRIVATE_KEY,
                            BUZZ_ANNOUNCE_REPO_OWNER and BUZZ_ANNOUNCE_REPO_ID.
                            Announcing is one-way: it never fails the deploy.
  --relay-restart <mode>    run.sh (default; force-recreates only the relay
                            container, leaving postgres/redis/minio untouched)
                            or systemd (systemctl restart of the relay unit,
                            which stops the whole compose project first).
  -h, --help                This.

Exit codes: 0 ok · 1 refused/blocked · 2 usage or bad manifest · 3 failed and
rolled back · 4 failed AND ROLLBACK FAILED.
MSG
}

die_usage() {
  emit usage FAIL "$1"
  usage >&2
  exit 2
}

# -----------------------------------------------------------------------------
# Failure handling.
#
# The ERR trap exists so that an unanticipated failure — a typo'd variable, a
# tool that vanished mid-run — takes the same road as an anticipated one. A
# deployer that rolls back on the failures it thought of and leaves a half-swap
# behind on the ones it did not is worse than no deployer.
# -----------------------------------------------------------------------------
on_err() {
  local code=$? line=$1
  trap - ERR
  step_fail "unexpected" "command failed at line ${line} (exit ${code})"
  deploy_failed "unexpected failure at line ${line}"
}
trap 'on_err "${LINENO}"' ERR

finish() {
  RESULT_CODE="$1"
  RESULT_REASON="$2"
  local mode="dry-run"
  ${EXECUTE} && mode="execute"
  emit summary "$([[ "${RESULT_CODE}" -eq 0 ]] && echo PASS || echo FAIL)" \
    "mode=${mode} passed=${PASS_COUNT} failed=${FAIL_COUNT} exit=${RESULT_CODE} reason=${RESULT_REASON}"
  exit "${RESULT_CODE}"
}

on_exit() {
  local code=$?
  trap - ERR EXIT
  # The inbox is only consumed by a real deploy. A dry run that swallowed the
  # release would mean you could never dry-run the thing you are about to ship.
  if [[ -n "${RELEASE_DIR}" ]] && ${EXECUTE}; then
    archive_release "${code}"
  fi
  rm -f "${TRANSCRIPT}"
  exit "${code}"
}

# -----------------------------------------------------------------------------
# Command execution.
#
# Every mutation funnels through run_mutation so dry-run is honoured in one
# place instead of at twenty call sites, and so the intent line is printed
# BEFORE the command runs. If the box loses power between the two, the journal
# still says what was in flight.
# -----------------------------------------------------------------------------
run_mutation() {
  local step="$1" detail="$2"
  shift 2
  if ! ${EXECUTE}; then
    step_plan "${step}" "${detail} :: would run: $*"
    return 0
  fi
  step_info "${step}" "${detail} :: running: $*"
  if ! "$@"; then
    step_fail "${step}" "${detail} :: command failed: $*"
    return 1
  fi
  return 0
}

# A preflight refusal. In --execute this stops the run before anything moves;
# in a dry run it is recorded and the walk continues, because the point of a
# dry run is the complete list of what would block you, not the first item.
refuse() {
  local step="$1" detail="$2"
  step_fail "${step}" "${detail}"
  BLOCKED=true
  if ${EXECUTE}; then
    finish 1 "preflight refused: ${step}"
  fi
}

# -----------------------------------------------------------------------------
# Announcements (Phase 1 commentary seam).
#
# One-way reporting, always. A relay that is down is exactly when you most want
# a deploy to proceed to its gates and its rollback, so an announcement failure
# is a WARN and never anything more. Note also the identity: the deployer signs
# with its OWN key, never an agent's, so a comment saying "deploy failed, rolled
# back" cannot be confused with an agent's own claim about its work.
# -----------------------------------------------------------------------------
announce() {
  local phase="$1" body="$2"
  [[ -n "${ANNOUNCE_ROOT}" ]] || return 0
  if [[ -z "${BUZZ_ANNOUNCE_REPO_OWNER:-}" || -z "${BUZZ_ANNOUNCE_REPO_ID:-}" ]]; then
    step_warn "announce.${phase}" "skipped: --announce-root given but BUZZ_ANNOUNCE_REPO_OWNER/BUZZ_ANNOUNCE_REPO_ID are unset"
    return 0
  fi
  if [[ ! -x "${BUZZ_CLI}" ]]; then
    step_warn "announce.${phase}" "skipped: ${BUZZ_CLI} is not executable"
    return 0
  fi
  if ! ${EXECUTE}; then
    step_plan "announce.${phase}" "would post to root=${ANNOUNCE_ROOT} via ${BUZZ_CLI} issues comment"
    return 0
  fi
  # `buzz issues comment` is the right verb: a project comment is a kind:1
  # carrying the repo `a` tag and the root `e` tag, and `buzz pr comment` is a
  # documented alias for the same event shape. So one code path serves a deploy
  # announced on an issue and one announced on a pull request.
  if timeout 30 "${BUZZ_CLI}" issues comment \
      --repo-owner "${BUZZ_ANNOUNCE_REPO_OWNER}" \
      --repo-id "${BUZZ_ANNOUNCE_REPO_ID}" \
      --root "${ANNOUNCE_ROOT}" \
      --content - <<<"${body}" >/dev/null 2>&1; then
    step_info "announce.${phase}" "posted to root=${ANNOUNCE_ROOT}"
  else
    step_warn "announce.${phase}" "post failed (ignored — announcing never fails a deploy)"
  fi
}

# -----------------------------------------------------------------------------
# Manifest.
# -----------------------------------------------------------------------------
load_manifest() {
  # The minter owns validation. Calling it here rather than reimplementing the
  # checks is the same discipline prepare-desktop-release.sh follows when it
  # calls desktop_release.py validate: one definition of "valid", used by both
  # the tool that writes and the tool that reads.
  #
  # --no-repo is the one check we take back, because it is the one check whose
  # right answer depends on where you are standing. At mint time you are in the
  # build worktree and HEAD equality is provable, so the minter demands it. At
  # deploy time the executor may be a systemd unit pointed at the shared object
  # store, where HEAD is whatever main happens to be. preflight_source_commit()
  # below states which of the two claims it was able to make.
  local out
  if ! out="$(python3 "${MINTER}" validate --manifest "${MANIFEST}" \
        --artifact-root "${ARTIFACT_ROOT}" --no-repo 2>&1)"; then
    step_fail "preflight.manifest" "$(printf '%s' "${out}" | tr '\n' ' ')"
    finish 2 "manifest invalid"
  fi
  step_pass "preflight.manifest" "${out}"

  local key value
  while IFS=$'\t' read -r key value; do
    MF["${key}"]="${value}"
  done < <(python3 - "${MANIFEST}" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
def out(k, v):
    print(f"{k}\t{v}")
out("name", m["name"])
out("source_commit", m["source_commit"])
out("migrations", m["migrations"])
out("previous_commit", m.get("previous_commit") or "")
out("deploy_order", " ".join(m["deploy_order"]))
comps = m["components"]
if "relay-image" in comps:
    out("relay_image", comps["relay-image"]["image"])
    out("relay_image_id", comps["relay-image"]["image_id"])
for c in ("acp", "cli"):
    if c in comps:
        out(f"{c}_artifact", comps[c]["artifact"])
        out(f"{c}_sha256", comps[c]["sha256"])
        out(f"{c}_bytes", str(comps[c]["bytes"]))
out("notes", " ".join((m.get("notes") or "").split()))
PY
  )

  read -r -a ORDER <<<"${MF[deploy_order]}"
  if ((${#ONLY_COMPONENTS[@]})); then
    local narrowed=() want found
    for want in "${ONLY_COMPONENTS[@]}"; do
      found=false
      for c in "${ORDER[@]}"; do [[ "$c" == "$want" ]] && found=true; done
      ${found} || die_usage "--component ${want} is not in the manifest; narrowing can only remove work"
    done
    for c in "${ORDER[@]}"; do
      for want in "${ONLY_COMPONENTS[@]}"; do
        [[ "$c" == "$want" ]] && narrowed+=("$c")
      done
    done
    ORDER=("${narrowed[@]}")
    step_info "preflight.narrow" "operator narrowed release to: ${ORDER[*]}"
  fi
  step_pass "preflight.plan" \
    "name=${MF[name]} commit=${MF[source_commit]:0:12} order=${ORDER[*]} migrations=${MF[migrations]}"
  [[ -n "${MF[notes]}" ]] && step_info "preflight.notes" "${MF[notes]}"
  return 0
}

has_component() {
  local want="$1" c
  for c in "${ORDER[@]}"; do [[ "$c" == "$want" ]] && return 0; done
  return 1
}

# -----------------------------------------------------------------------------
# Preflight. Every step here is read-only and idempotent: running preflight
# twice in a row produces the same answer and changes nothing, which is what
# makes it safe to run it from a dry run, from a real deploy, and from a human
# poking at a suspicious release.
# -----------------------------------------------------------------------------
preflight_tooling() {
  local missing=() tool
  for tool in python3 git docker systemctl curl sha256sum install mv sed df journalctl timeout; do
    command -v "${tool}" >/dev/null 2>&1 || missing+=("${tool}")
  done
  if ((${#missing[@]})); then
    refuse "preflight.tooling" "missing required tools: ${missing[*]}"
    return 0
  fi
  step_pass "preflight.tooling" "all required tools present"
}

preflight_layout() {
  local missing=() path
  for path in "${RELAY_DIR}" "${RELAY_ENV}" "${BIN_DIR}"; do
    [[ -e "${path}" ]] || missing+=("${path}")
  done
  if ((${#missing[@]})); then
    refuse "preflight.layout" "deployment layout is not what this runbook describes; missing: ${missing[*]}"
    return 0
  fi
  step_pass "preflight.layout" "root=${BUZZ_ROOT} relay=${RELAY_DIR} bin=${BIN_DIR}"
}

preflight_source_commit() {
  # Two strengths of the same check, and the script says out loud which one it
  # got.
  #
  # The strong form is HEAD equality: run from the worktree the artifacts were
  # built in, `HEAD == source_commit` proves the manifest describes this tree.
  # That is what a human gets when they run this from their build worktree.
  #
  # The weak form is existence. Under systemd the deployer is detached from any
  # particular worktree — BUZZ_DEPLOY_REPO points at /opt/buzz/src, the shared
  # object store every worktree hangs off — so HEAD is whatever main happens to
  # be. There, all this check can honestly claim is that source_commit names a
  # real, fetched commit rather than forty invented hex digits. Say that,
  # rather than quietly weakening the strong claim: what actually binds the
  # manifest to the bytes being installed is the artifact hash, and pretending
  # otherwise is how a check becomes decoration.
  local head
  if ! head="$(git -C "${REPO}" rev-parse HEAD 2>/dev/null)"; then
    refuse "preflight.source_commit" "cannot read HEAD of build repo ${REPO}"
    return 0
  fi
  if [[ "${head}" == "${MF[source_commit]}" ]]; then
    step_pass "preflight.source_commit" "${REPO} HEAD == manifest source_commit ${head:0:12} (built here)"
    return 0
  fi
  if git -C "${REPO}" cat-file -e "${MF[source_commit]}^{commit}" 2>/dev/null; then
    local where
    # Strip git's current-branch/other-worktree markers ('*' and '+'); they are
    # true of the repo, not of the commit, and only confuse the report.
    where="$(git -C "${REPO}" branch --all --contains "${MF[source_commit]}" 2>/dev/null \
             | sed -E 's/^[*+ ]+//' | head -3 | tr '\n' ' ')"
    step_warn "preflight.source_commit" \
      "${REPO} is at ${head:0:12}, not the manifest's ${MF[source_commit]:0:12}: this deployer is not running in the build tree. The commit is real and present${where:+ (contained in:${where%% })}; artifact hashes are the binding check."
    step_pass "preflight.source_commit" "manifest source_commit ${MF[source_commit]:0:12} exists in ${REPO}"
    return 0
  fi
  refuse "preflight.source_commit" \
    "manifest source_commit ${MF[source_commit]:0:12} does not exist in ${REPO}; this release names a tree the box has never seen"
}

preflight_no_fetch() {
  # Belt and braces on rule 3. The schema already confines relay-image to the
  # buzz-local: namespace and artifacts to relative paths, but stating the rule
  # again where the fetch would happen means a future schema relaxation cannot
  # quietly grant a network hop.
  local bad=""
  if has_component relay-image && [[ "${MF[relay_image]}" != buzz-local:* ]]; then
    bad="image ${MF[relay_image]} is not in the local-only buzz-local: namespace"
  fi
  if grep -Eq '"artifact"[[:space:]]*:[[:space:]]*"[a-z]+://' "${MANIFEST}"; then
    bad="manifest contains a URL-shaped artifact reference"
  fi
  if [[ -n "${bad}" ]]; then
    refuse "preflight.no_fetch" "${bad}; this deployer never fetches, artifacts must already be here"
    return 0
  fi
  step_pass "preflight.no_fetch" "all artifacts are local; no registry or network fetch will be attempted"
}

preflight_artifacts() {
  # The minter already hashed these during load_manifest. Re-stating the result
  # as its own step matters for the journal: "the bytes I am about to install
  # are the bytes that were tested" deserves its own line, not a footnote in a
  # validator's output.
  local comp path actual
  for comp in acp cli; do
    has_component "${comp}" || continue
    path="${ARTIFACT_ROOT}/${MF[${comp}_artifact]}"
    if [[ ! -f "${path}" ]]; then
      refuse "preflight.artifact.${comp}" "artifact ${path} does not exist"
      continue
    fi
    actual="$(sha256sum "${path}" | cut -d' ' -f1)"
    if [[ "${actual}" != "${MF[${comp}_sha256]}" ]]; then
      refuse "preflight.artifact.${comp}" \
        "${path} hashes to ${actual:0:16} but the manifest says ${MF[${comp}_sha256]:0:16}"
      continue
    fi
    step_pass "preflight.artifact.${comp}" \
      "${path} sha256=${actual:0:16}… bytes=${MF[${comp}_bytes]} matches manifest"
  done

  if has_component relay-image; then
    local id
    if ! id="$(docker image inspect --format '{{.Id}}' "${MF[relay_image]}" 2>/dev/null)"; then
      refuse "preflight.artifact.relay-image" \
        "image ${MF[relay_image]} is not in the local docker daemon; build it, do not pull it"
      return 0
    fi
    if [[ "${id}" != "${MF[relay_image_id]}" ]]; then
      refuse "preflight.artifact.relay-image" \
        "tag ${MF[relay_image]} resolves to ${id:0:19}… but the manifest recorded ${MF[relay_image_id]:0:19}…; the tag moved after minting"
      return 0
    fi
    step_pass "preflight.artifact.relay-image" "${MF[relay_image]} == ${id:0:19}… matches manifest"
  fi
}

live_image() {
  sed -n 's/^BUZZ_IMAGE=//p' "${RELAY_ENV}" 2>/dev/null | head -1
}

preflight_migrations() {
  PREVIOUS_IMAGE="$(live_image || true)"
  if [[ -z "${PREVIOUS_IMAGE}" ]]; then
    refuse "preflight.migrations" "no BUZZ_IMAGE in ${RELAY_ENV}; this deployment is not shaped the way this runbook assumes"
    return 0
  fi
  local short="${PREVIOUS_IMAGE##*-}"
  if git -C "${REPO}" rev-parse --verify --quiet "${short}^{commit}" >/dev/null 2>&1; then
    PREVIOUS_COMMIT="$(git -C "${REPO}" rev-parse "${short}^{commit}")"
  fi

  if [[ -z "${PREVIOUS_COMMIT}" ]]; then
    # Cannot compute the delta, so cannot grant the fast lane. Treating an
    # unknown baseline as "probably fine" is how a schema change ships as a
    # schema-neutral release.
    if [[ "${MF[migrations]}" == "none" ]]; then
      refuse "preflight.migrations" \
        "live image ${PREVIOUS_IMAGE} does not resolve to a commit in ${REPO}, so the migrations delta cannot be verified; a manifest claiming migrations=none cannot be trusted here"
      return 0
    fi
    step_warn "preflight.migrations" \
      "live commit unknown (image ${PREVIOUS_IMAGE}); relying on the manifest's declared class ${MF[migrations]}"
  else
    local delta count
    delta="$(git -C "${REPO}" diff --name-only "${PREVIOUS_COMMIT}..${MF[source_commit]}" -- migrations/ || true)"
    count="$(printf '%s' "${delta}" | grep -c . || true)"
    step_info "preflight.migrations" \
      "live=${PREVIOUS_COMMIT:0:12} candidate=${MF[source_commit]:0:12} migrations_delta=${count} file(s)"
    if ((count > 0)); then
      step_info "preflight.migrations" "delta: $(printf '%s' "${delta}" | tr '\n' ' ')"
      if [[ "${MF[migrations]}" == "none" ]]; then
        refuse "preflight.migrations" \
          "manifest says migrations=none but git shows ${count} changed migration file(s); the manifest understates reality"
        return 0
      fi
    else
      # Overstating is always safe — a manifest that asks for a full backup it
      # did not need costs a few minutes; the opposite costs the database.
      if [[ "${MF[migrations]}" != "none" ]]; then
        step_info "preflight.migrations" \
          "delta is empty but the manifest declares ${MF[migrations]}; honouring the stricter class"
      else
        step_pass "preflight.migrations" "schema-neutral fast lane: migrations/ delta is empty"
        return 0
      fi
    fi
  fi

  case "${MF[migrations]}" in
    backward-safe)
      step_pass "preflight.migrations" \
        "declared backward-safe: every migration is expand-only, so the previous binary still runs against the new schema and a binary rollback is clean. A fresh full backup is still required."
      ;;
    ack-required)
      if ! ${ACK_MIGRATIONS}; then
        refuse "preflight.migrations" \
          "manifest declares migrations=ack-required and --ack-migrations was not passed. Rollback after this release will NOT restore service without a database restore. The acknowledgement is a command-line act on purpose: an unattended inbox drop must never run a forward-only migration by itself."
        return 0
      fi
      step_pass "preflight.migrations" \
        "ack-required acknowledged on the command line; BINARY ROLLBACK WILL NOT UNDO THIS SCHEMA CHANGE"
      ;;
    none)
      step_pass "preflight.migrations" "declared none"
      ;;
  esac
}

preflight_disk() {
  local need_kib avail_kib comp bytes=0
  for comp in acp cli; do
    has_component "${comp}" && bytes=$((bytes + MF[${comp}_bytes]))
  done
  # Twice the artifact size (the backup copy plus the rollback copy) plus a
  # floor, because a deploy that fills the disk while writing its own escape
  # hatch is the worst possible ordering of events.
  need_kib=$(( (bytes * 2) / 1024 + MIN_FREE_MIB * 1024 ))
  avail_kib="$(df -Pk "${BUZZ_ROOT}" | awk 'NR==2 {print $4}')"
  if [[ -z "${avail_kib}" ]]; then
    refuse "preflight.disk" "cannot read free space for ${BUZZ_ROOT}"
    return 0
  fi
  if ((avail_kib < need_kib)); then
    refuse "preflight.disk" \
      "${BUZZ_ROOT} has $((avail_kib / 1024)) MiB free, needs $((need_kib / 1024)) MiB for backups plus headroom"
    return 0
  fi
  step_pass "preflight.disk" "$((avail_kib / 1024)) MiB free on ${BUZZ_ROOT}, need $((need_kib / 1024)) MiB"
}

preflight_stack() {
  # Refuse to deploy onto an already-broken stack. Not politeness: if the relay
  # is unhealthy before you start, every gate below becomes unreadable, because
  # you cannot tell your breakage from the breakage that was already there —
  # and rollback would "restore" a state that was never working.
  local status
  status="$(docker ps --filter "label=com.docker.compose.project=${COMPOSE_PROJECT}" \
                      --filter "label=com.docker.compose.service=relay" \
                      --format '{{.Status}}' 2>/dev/null || true)"
  if [[ "${status}" != *"(healthy)"* ]]; then
    refuse "preflight.stack" "relay container is not healthy before we start (status: ${status:-absent})"
    return 0
  fi
  step_pass "preflight.stack" "relay container ${status}"

  local body
  if ! body="$(curl --fail --silent --show-error --max-time 10 "${LIVENESS_LOCAL}" 2>&1)" || [[ "${body}" != "ok" ]]; then
    refuse "preflight.stack" "${LIVENESS_LOCAL} did not answer ok (got: ${body})"
    return 0
  fi
  step_pass "preflight.stack" "${LIVENESS_LOCAL} == ok"

  if ! body="$(curl --fail --silent --show-error --max-time 15 "${LIVENESS_PUBLIC}" 2>&1)" || [[ "${body}" != "ok" ]]; then
    refuse "preflight.stack" "${LIVENESS_PUBLIC} did not answer ok (got: ${body})"
    return 0
  fi
  step_pass "preflight.stack" "${LIVENESS_PUBLIC} == ok"

  if has_component acp; then
    local unit
    for unit in "${AGENT_UNITS[@]}"; do
      if [[ "$(systemctl is-active "${unit}" 2>/dev/null || true)" != "active" ]]; then
        refuse "preflight.stack" "${unit} is not active before we start; fix it first, then deploy"
        return 0
      fi
      step_pass "preflight.stack" "${unit} is active"
    done
  fi
}

# -----------------------------------------------------------------------------
# PHASE 2 SEAM — authorization.
#
# This function is the single place a later phase wires owner approval in. It
# is deliberately one function with one caller, so the change is a diff you can
# read in one sitting rather than a policy sprinkled through the runbook.
#
# Phase 1 posture: promoted_by must be null. An unverified promotion claim is
# refused rather than ignored, because a manifest asserting an approval that
# nothing checked will read like authority to the next person who greps the
# audit log. The minter enforces the same rule; both ends state it so neither
# can quietly become the exception.
# -----------------------------------------------------------------------------
gate_authorization() {
  step_skip "preflight.authorization" \
    "Phase 1: no authorization policy is installed. The manifest's promoted_by is required to be null (enforced by mint-manifest.py). Phase 2 replaces this function with signature verification against the owner pubkey."
}

# -----------------------------------------------------------------------------
# Backups.
# -----------------------------------------------------------------------------
backup_current_state() {
  local stamp
  stamp="$(date -u '+%Y%m%dT%H%M%SZ')"
  BACKUP_DIR="${BACKUP_ROOT}/${MF[name]}-${stamp}"

  # A backup directory is named for the release it precedes; the rollback
  # binary is named for what it CONTAINS. Those are different facts and
  # conflating them is how someone restores the wrong binary at 3am. The live
  # image tag is the best available description of what is currently installed.
  local desc="${PREVIOUS_IMAGE#*:}"
  [[ -n "${desc}" ]] || desc="pre-${MF[name]}-${stamp}"
  ROLLBACK_BIN="${BIN_DIR}/buzz-acp.rollback-${desc}"

  run_mutation "backup.mkdir" "create ${BACKUP_DIR}" install -d -m 0700 "${BACKUP_DIR}" || return 1
  run_mutation "backup.mkdir" "create ${BACKUP_DIR}/bin" install -d -m 0700 "${BACKUP_DIR}/bin" || return 1
  run_mutation "backup.mkdir" "create ${BACKUP_DIR}/relay" install -d -m 0700 "${BACKUP_DIR}/relay" || return 1

  local src
  for src in "${BIN_DIR}/buzz-acp" "${BIN_DIR}/buzz"; do
    [[ -f "${src}" ]] || continue
    run_mutation "backup.copy" "preserve $(basename "${src}")" \
      cp -a "${src}" "${BACKUP_DIR}/bin/$(basename "${src}")" || return 1
  done
  run_mutation "backup.copy" "preserve relay/.env (BUZZ_IMAGE=${PREVIOUS_IMAGE})" \
    cp -a "${RELAY_ENV}" "${BACKUP_DIR}/relay/.env" || return 1

  if has_component acp && [[ -f "${BIN_DIR}/buzz-acp" ]]; then
    # A second copy, in /opt/buzz/bin, named for its contents. The backup dir is
    # the record; this is the thing a human reaches for without reading records.
    run_mutation "backup.rollback-copy" "keep ${ROLLBACK_BIN} beside the live binary" \
      cp -a "${BIN_DIR}/buzz-acp" "${ROLLBACK_BIN}" || return 1
  else
    # Say so rather than leaving the rollback copy's absence to be inferred:
    # a release that is not replacing buzz-acp has no buzz-acp to preserve.
    ROLLBACK_BIN=""
  fi

  if ${EXECUTE}; then
    # Verify after. A backup nobody checked is a rumour.
    ( cd "${BACKUP_DIR}" && find . -type f ! -name SHA256SUMS -print0 \
        | sort -z | xargs -0 sha256sum >SHA256SUMS ) || {
      step_fail "backup.verify" "could not write ${BACKUP_DIR}/SHA256SUMS"
      return 1
    }
    if ( cd "${BACKUP_DIR}" && sha256sum --quiet --check SHA256SUMS ); then
      step_pass "backup.verify" \
        "${BACKUP_DIR} verified ($(grep -c . "${BACKUP_DIR}/SHA256SUMS") files); rollback binary: ${ROLLBACK_BIN:-none needed, buzz-acp is not in this release}"
    else
      step_fail "backup.verify" "${BACKUP_DIR} failed its own checksum manifest"
      return 1
    fi
  else
    step_plan "backup.verify" "would write and re-verify ${BACKUP_DIR}/SHA256SUMS, and keep ${ROLLBACK_BIN}"
  fi
  return 0
}

full_backup() {
  # Postgres-inclusive backup, required whenever the schema is about to move.
  # Binary rollback restores code; only this restores data, and a migration is
  # the one kind of change that makes those two different things.
  if [[ "${MF[migrations]}" == "none" ]]; then
    step_skip "backup.full" "schema-neutral release; the binary/env backup above is the whole rollback story"
    return 0
  fi
  if [[ ! -x "${FULL_BACKUP_SCRIPT}" ]]; then
    step_fail "backup.full" "${FULL_BACKUP_SCRIPT} is missing or not executable, and this release changes the schema"
    return 1
  fi
  local marker="${BACKUP_ROOT}/latest/BACKUP.json"
  if [[ -f "${marker}" ]]; then
    local age_min
    age_min=$(( ( $(date +%s) - $(stat -c %Y "${marker}") ) / 60 ))
    if ((age_min < FULL_BACKUP_MAX_AGE_MIN)); then
      step_skip "backup.full" "${BACKUP_ROOT}/latest is ${age_min} min old (< ${FULL_BACKUP_MAX_AGE_MIN}); reusing it"
      return 0
    fi
    step_info "backup.full" "${BACKUP_ROOT}/latest is ${age_min} min old; taking a fresh one"
  fi
  run_mutation "backup.full" "postgres-inclusive backup before a schema change" \
    "${FULL_BACKUP_SCRIPT}" || return 1
  # Claim the backup only when one was actually taken. A dry run that reports
  # "fresh full backup taken" is precisely the kind of comfortable lie this
  # whole script exists to avoid.
  ${EXECUTE} && step_pass "backup.full" "fresh full backup taken at ${BACKUP_ROOT}/latest"
  return 0
}

# -----------------------------------------------------------------------------
# Component: relay image.
# -----------------------------------------------------------------------------
set_relay_image() {
  local target="$1" step="$2"
  if [[ "$(live_image || true)" == "${target}" ]]; then
    step_skip "${step}" "BUZZ_IMAGE is already ${target}; nothing to write"
    return 0
  fi
  if ! grep -q '^BUZZ_IMAGE=' "${RELAY_ENV}"; then
    step_fail "${step}" "${RELAY_ENV} has no BUZZ_IMAGE line to replace; refusing to invent one"
    return 1
  fi
  if ! ${EXECUTE}; then
    step_plan "${step}" "would rewrite BUZZ_IMAGE in ${RELAY_ENV}: $(live_image) -> ${target}"
    return 0
  fi
  # Write a sibling temp file and rename over the original: rename(2) within a
  # directory is atomic, so a crash mid-write can never leave the relay with a
  # half-written .env full of database passwords.
  local tmp
  tmp="$(mktemp "${RELAY_ENV}.deploy.XXXXXX")"
  sed -E "s|^BUZZ_IMAGE=.*|BUZZ_IMAGE=${target}|" "${RELAY_ENV}" >"${tmp}"
  chown --reference="${RELAY_ENV}" "${tmp}"
  chmod --reference="${RELAY_ENV}" "${tmp}"
  mv -f "${tmp}" "${RELAY_ENV}"
  local wrote
  wrote="$(live_image || true)"
  if [[ "${wrote}" != "${target}" ]]; then
    step_fail "${step}" "wrote BUZZ_IMAGE but ${RELAY_ENV} now reads ${wrote}"
    return 1
  fi
  step_pass "${step}" "BUZZ_IMAGE=${target} written and read back"
  return 0
}

restart_relay() {
  local step="$1"
  case "${RELAY_RESTART_MODE}" in
    run.sh)
      # ./run.sh restart is `compose up -d --wait --force-recreate relay`: it
      # recreates ONLY the relay container. Postgres, Redis and MinIO keep
      # running, so a code deploy does not become a database restart. The
      # buzz-relay.service unit stays active across this (Type=oneshot,
      # RemainAfterExit=yes) and the tailscale serve mapping lives in
      # tailscaled state, not in the container, so it survives too — which the
      # public liveness gate below then proves rather than assumes.
      run_mutation "${step}" "force-recreate the relay container only" \
        "${RELAY_RUN}" restart || return 1
      ;;
    systemd)
      run_mutation "${step}" "systemctl restart ${RELAY_UNIT} (stops the whole compose project first)" \
        systemctl restart "${RELAY_UNIT}" || return 1
      ;;
    *)
      step_fail "${step}" "unknown --relay-restart mode ${RELAY_RESTART_MODE}"
      return 1
      ;;
  esac
  DID_RESTART_RELAY=true
  return 0
}

gate_relay() {
  local step="$1" expect_image="$2"
  if ! ${EXECUTE}; then
    step_plan "${step}" "would poll for up to ${RELAY_GATE_SECONDS}s: docker relay container (healthy) and running ${expect_image:-<unchanged>}; ${LIVENESS_LOCAL} == ok; ${LIVENESS_PUBLIC} == ok"
    return 0
  fi
  local deadline=$(( $(date +%s) + RELAY_GATE_SECONDS ))
  local status image local_body public_body
  while :; do
    status="$(docker ps --filter "label=com.docker.compose.project=${COMPOSE_PROJECT}" \
                        --filter "label=com.docker.compose.service=relay" \
                        --format '{{.Status}}' 2>/dev/null || true)"
    image="$(docker ps --filter "label=com.docker.compose.project=${COMPOSE_PROJECT}" \
                       --filter "label=com.docker.compose.service=relay" \
                       --format '{{.Image}}' 2>/dev/null || true)"
    local_body="$(curl --fail --silent --max-time 5 "${LIVENESS_LOCAL}" 2>/dev/null || true)"
    public_body="$(curl --fail --silent --max-time 10 "${LIVENESS_PUBLIC}" 2>/dev/null || true)"
    if [[ "${status}" == *"(healthy)"* && "${local_body}" == "ok" && "${public_body}" == "ok" ]] \
       && { [[ -z "${expect_image}" ]] || [[ "${image}" == "${expect_image}" ]]; }; then
      step_pass "${step}" "container ${status} running ${image}; local liveness ok; tailnet liveness ok"
      return 0
    fi
    if (( $(date +%s) >= deadline )); then
      step_fail "${step}" \
        "after ${RELAY_GATE_SECONDS}s: container='${status:-absent}' image='${image:-absent}' expected='${expect_image:-<any>}' local='${local_body:-<no answer>}' tailnet='${public_body:-<no answer>}'"
      return 1
    fi
    sleep 3
  done
}

# -----------------------------------------------------------------------------
# Component: binaries.
# -----------------------------------------------------------------------------
install_binary() {
  local comp="$1" step="$2" src="$3"
  local dst="${INSTALL_PATH[${comp}]}" owner="${INSTALL_OWNER[${comp}]}" mode="${INSTALL_MODE[${comp}]}"

  # Re-hash immediately before install. The preflight hash proved the artifact
  # was right some minutes ago; the interesting window is between then and now,
  # and closing it costs one second on a 17 MB file.
  if ${EXECUTE}; then
    local actual
    actual="$(sha256sum "${src}" | cut -d' ' -f1)"
    if [[ "${actual}" != "${MF[${comp}_sha256]}" ]]; then
      step_fail "${step}" "artifact changed under us: ${src} is now ${actual:0:16}, manifest says ${MF[${comp}_sha256]:0:16}"
      return 1
    fi
  fi

  if ! ${EXECUTE}; then
    step_plan "${step}" "would install ${src} -> ${dst} owner=${owner} mode=${mode} (via temp + atomic rename)"
    return 0
  fi

  # install(1) opens the destination with O_TRUNC, which fails with ETXTBSY on
  # a binary that is currently executing — and buzz-acp is running right now.
  # Install beside the target and rename over it instead: the running process
  # keeps its old inode until it is restarted, and the swap is atomic.
  local tmp="${dst}.deploy-$$"
  step_info "${step}" "installing ${src} -> ${dst} owner=${owner} mode=${mode}"
  if ! install -o "${owner%%:*}" -g "${owner##*:}" -m "${mode}" "${src}" "${tmp}"; then
    step_fail "${step}" "install to ${tmp} failed"
    rm -f "${tmp}"
    return 1
  fi
  if ! mv -f "${tmp}" "${dst}"; then
    step_fail "${step}" "atomic rename ${tmp} -> ${dst} failed"
    rm -f "${tmp}"
    return 1
  fi
  local check
  check="$(sha256sum "${dst}" | cut -d' ' -f1)"
  if [[ "${check}" != "${MF[${comp}_sha256]}" ]]; then
    step_fail "${step}" "installed ${dst} hashes to ${check:0:16}, expected ${MF[${comp}_sha256]:0:16}"
    return 1
  fi
  step_pass "${step}" "${dst} installed, sha256 ${check:0:16}… verified in place, $(stat -c '%U:%G %a' "${dst}")"
  return 0
}

gate_agent() {
  local unit="$1" step="$2" cursor="$3"
  if ! ${EXECUTE}; then
    step_plan "${step}" "would watch ${unit} for ${AGENT_GATE_SECONDS}s requiring: $(printf '%s | ' "${AGENT_REQUIRED_LINES[@]}")and zero ERROR lines"
    return 0
  fi
  local deadline=$(( $(date +%s) + AGENT_GATE_SECONDS ))
  local logs missing pattern errors
  while :; do
    logs="$(journal_since "${unit}" "${cursor}")"
    # Fail fast on either of the two unrecoverable signals: an ERROR line, or a
    # unit that is no longer running. Waiting out the full window to confirm
    # what we already know only delays the rollback.
    errors="$(printf '%s\n' "${logs}" | grep -Ec "${AGENT_ERROR_PATTERN}" || true)"
    if ((errors > 0)); then
      step_fail "${step}" "${unit} logged ${errors} ERROR line(s): $(printf '%s\n' "${logs}" | grep -E "${AGENT_ERROR_PATTERN}" | head -3 | tr '\n' ' ')"
      return 1
    fi
    if [[ "$(systemctl is-active "${unit}" 2>/dev/null || true)" != "active" ]]; then
      step_fail "${step}" "${unit} is not active during its gate window"
      return 1
    fi
    missing=()
    for pattern in "${AGENT_REQUIRED_LINES[@]}"; do
      printf '%s\n' "${logs}" | grep -qF "${pattern}" || missing+=("${pattern}")
    done
    if ((${#missing[@]} == 0)); then
      # The four lines are present and nothing has errored. Ride out the rest
      # of the window anyway: "started cleanly" is a claim about the first 60
      # seconds, and a crash at second 45 is exactly the kind this catches.
      if (( $(date +%s) >= deadline )); then
        step_pass "${step}" "${unit} produced all ${#AGENT_REQUIRED_LINES[@]} required lines and zero ERROR lines in ${AGENT_GATE_SECONDS}s"
        return 0
      fi
    elif (( $(date +%s) >= deadline )); then
      step_fail "${step}" "${unit} never logged: $(printf '%s; ' "${missing[@]}")"
      return 1
    fi
    sleep 3
  done
}

journal_cursor() {
  # A cursor is exact where a timestamp is merely close: --since has one-second
  # resolution, and a line written in the same second as the restart would be
  # attributed to the new process. Fall back to a timestamp when the unit has
  # no journal yet (first ever start).
  local unit="$1" cursor
  cursor="$(journalctl -u "${unit}" -n 1 --show-cursor -o cat 2>/dev/null \
            | sed -n 's/^-- cursor: //p' | tail -1)"
  if [[ -n "${cursor}" ]]; then
    printf 'cursor:%s' "${cursor}"
  else
    printf 'since:%s' "$(date '+%Y-%m-%d %H:%M:%S')"
  fi
}

journal_since() {
  local unit="$1" mark="$2"
  case "${mark}" in
    cursor:*) journalctl -u "${unit}" --after-cursor "${mark#cursor:}" --no-pager -o cat 2>/dev/null || true ;;
    since:*)  journalctl -u "${unit}" --since "${mark#since:}" --no-pager -o cat 2>/dev/null || true ;;
    *)        journalctl -u "${unit}" -n 200 --no-pager -o cat 2>/dev/null || true ;;
  esac
}

restart_agents() {
  # One leg at a time. Restarting both agents together halves the wall clock
  # and doubles the blast radius: if the new binary is bad, the second unit is
  # already down before the first one's gate has said so. Restarting them in
  # sequence means a failure leaves one working agent and a clean rollback.
  local unit step cursor
  for unit in "${AGENT_UNITS[@]}"; do
    step="acp.restart.${unit%.service}"
    cursor="$(journal_cursor "${unit}")"
    run_mutation "${step}" "restart ${unit} onto the new buzz-acp" \
      systemctl restart "${unit}" || return 1
    ${EXECUTE} && RESTARTED_AGENTS+=("${unit}")
    gate_agent "${unit}" "acp.gate.${unit%.service}" "${cursor}" || return 1
    announce "agent" "Deploy \`${MF[name]}\`: \`${unit}\` restarted and passed its startup gate."
  done
  return 0
}

gate_cli() {
  local step="cli.gate"
  if ! ${EXECUTE}; then
    step_plan "${step}" "would run ${INSTALL_PATH[cli]} --help to prove the new binary loads"
    return 0
  fi
  # A smoke test, not a feature test: it proves the binary is the right
  # architecture, its dynamic links resolve and its argument parser builds.
  # Anything deeper belongs in the staging gates, not in a deployer.
  if timeout 20 "${INSTALL_PATH[cli]}" --help >/dev/null 2>&1; then
    step_pass "${step}" "${INSTALL_PATH[cli]} --help exits 0"
    return 0
  fi
  step_fail "${step}" "${INSTALL_PATH[cli]} --help did not exit 0"
  return 1
}

# -----------------------------------------------------------------------------
# Rollback.
#
# Restores exactly what this run changed, in reverse order, then re-gates. The
# re-gate matters: a rollback that is not verified is a second unverified
# deploy performed under worse conditions than the first.
# -----------------------------------------------------------------------------
rollback() {
  trap - ERR   # rollback handles its own failures; the trap would recurse
  local ok=true
  emit rollback INFO "restoring the state this run replaced (backup: ${BACKUP_DIR:-<none taken>})"
  announce "rollback" "Deploy \`${MF[name]:-?}\` FAILED — rolling back to \`${PREVIOUS_IMAGE:-previous}\`."

  if [[ "${MF[migrations]:-none}" != "none" ]]; then
    step_warn "rollback.migrations" \
      "this release carried migrations=${MF[migrations]}. Binary rollback does not roll back schema. For backward-safe migrations the restored binary runs fine against the migrated schema; for ack-required it does NOT, and you need ${BACKUP_ROOT}/latest and a database restore."
  fi

  if ${DID_INSTALL_CLI} && [[ -f "${BACKUP_DIR}/bin/buzz" ]]; then
    if cp -a "${BACKUP_DIR}/bin/buzz" "${INSTALL_PATH[cli]}.rollback-$$" \
       && mv -f "${INSTALL_PATH[cli]}.rollback-$$" "${INSTALL_PATH[cli]}"; then
      step_pass "rollback.cli" "restored ${INSTALL_PATH[cli]} from ${BACKUP_DIR}"
    else
      step_fail "rollback.cli" "could not restore ${INSTALL_PATH[cli]} from ${BACKUP_DIR}"
      ok=false
    fi
  fi

  if ${DID_INSTALL_ACP} && [[ -f "${BACKUP_DIR}/bin/buzz-acp" ]]; then
    if cp -a "${BACKUP_DIR}/bin/buzz-acp" "${INSTALL_PATH[acp]}.rollback-$$" \
       && mv -f "${INSTALL_PATH[acp]}.rollback-$$" "${INSTALL_PATH[acp]}"; then
      step_pass "rollback.acp" "restored ${INSTALL_PATH[acp]} from ${BACKUP_DIR}"
    else
      step_fail "rollback.acp" "could not restore ${INSTALL_PATH[acp]} from ${BACKUP_DIR}"
      ok=false
    fi
  fi

  if ${DID_SWAP_IMAGE} && [[ -n "${PREVIOUS_IMAGE}" ]]; then
    if set_relay_image "${PREVIOUS_IMAGE}" "rollback.image"; then
      :
    else
      step_fail "rollback.image" "could not restore BUZZ_IMAGE=${PREVIOUS_IMAGE}"
      ok=false
    fi
  fi

  # Restart in deploy order again — relay first, then agents — because the
  # ordering rule is a property of the system, not of the direction of travel.
  if ${DID_RESTART_RELAY} || ${DID_SWAP_IMAGE}; then
    if restart_relay "rollback.relay.restart" && gate_relay "rollback.relay.gate" "${PREVIOUS_IMAGE}"; then
      :
    else
      step_fail "rollback.relay" "the relay did not come back on the previous image"
      ok=false
    fi
  fi

  local unit cursor
  for unit in "${RESTARTED_AGENTS[@]}"; do
    cursor="$(journal_cursor "${unit}")"
    if systemctl restart "${unit}" && gate_agent "${unit}" "rollback.gate.${unit%.service}" "${cursor}"; then
      step_pass "rollback.agent" "${unit} restored and gated"
    else
      step_fail "rollback.agent" "${unit} did not come back cleanly on the previous binary"
      ok=false
    fi
  done

  ${ok} && return 0
  return 1
}

deploy_failed() {
  local reason="$1"
  trap - ERR
  emit deploy FAIL "${reason}"
  if ! ${EXECUTE}; then
    finish 1 "${reason}"
  fi
  # Nothing was mutated, so there is nothing to roll back and claiming a
  # rollback would misreport the box's state. Exit code 1 means "untouched",
  # and that distinction is the first thing anyone reading the journal needs.
  if ! ${DID_SWAP_IMAGE} && ! ${DID_INSTALL_ACP} && ! ${DID_INSTALL_CLI} \
     && ! ${DID_RESTART_RELAY} && ((${#RESTARTED_AGENTS[@]} == 0)); then
    emit rollback SKIP "nothing had been mutated yet; the deployment is untouched"
    finish 1 "${reason} (nothing mutated)"
  fi
  if rollback; then
    announce "result" "Deploy \`${MF[name]:-?}\` FAILED and was rolled back cleanly. Reason: ${reason}. The stack is back on \`${PREVIOUS_IMAGE:-the previous image}\`."
    emit rollback PASS "state restored and re-gated"
    finish 3 "${reason} (rolled back)"
  fi
  announce "result" "Deploy \`${MF[name]:-?}\` FAILED **and rollback did not complete**. Reason: ${reason}. A human must intervene: backup at ${BACKUP_DIR:-<none>}."
  emit rollback FAIL "ROLLBACK INCOMPLETE — manual intervention required; backup is at ${BACKUP_DIR:-<none>}"
  finish 4 "${reason} (ROLLBACK FAILED)"
}

# -----------------------------------------------------------------------------
# Inbox mode.
# -----------------------------------------------------------------------------
select_release() {
  local incoming="${INBOX_ROOT}/incoming"
  if [[ ! -d "${incoming}" ]]; then
    emit inbox SKIP "no ${incoming}; nothing to do"
    finish 0 "inbox empty"
  fi
  # Oldest first, one per run. The .path unit re-triggers while any release
  # remains, so a queue drains one release at a time — which is the only way to
  # keep the "relay first, gate, then agents" ordering meaningful.
  local candidate=""
  while IFS= read -r path; do
    candidate="${path}"
    break
  done < <(find "${incoming}" -mindepth 2 -maxdepth 2 -name manifest.json -printf '%T@ %p\n' 2>/dev/null \
           | sort -n | cut -d' ' -f2-)
  if [[ -z "${candidate}" ]]; then
    emit inbox SKIP "no complete release under ${incoming}"
    finish 0 "inbox empty"
  fi
  MANIFEST="${candidate}"
  RELEASE_DIR="$(dirname "${candidate}")"
  ARTIFACT_ROOT="${RELEASE_DIR}"
  emit inbox INFO "picked ${RELEASE_DIR}"
}

archive_release() {
  # Either way — success or failure — the release leaves the inbox and takes a
  # verdict with it. A release that stays in incoming/ after being attempted
  # would be re-attempted on the next path trigger, forever.
  local code="$1"
  local stamp processed
  stamp="$(date -u '+%Y%m%dT%H%M%SZ')"
  processed="${INBOX_ROOT}/processed/$(basename "${RELEASE_DIR}")-${stamp}"
  mkdir -p "${INBOX_ROOT}/processed" || return 0
  python3 - "${RELEASE_DIR}/result.json" "${code}" "${RESULT_REASON}" "${MANIFEST}" "${TRANSCRIPT}" <<'PY' || true
import json, sys, pathlib
out, code, reason, manifest, transcript = sys.argv[1:6]
try:
    name = json.loads(pathlib.Path(manifest).read_text()).get("name")
except Exception:
    name = None
try:
    lines = pathlib.Path(transcript).read_text().splitlines()
except Exception:
    lines = []
pathlib.Path(out).write_text(json.dumps({
    "name": name,
    "exit_code": int(code),
    "outcome": "success" if int(code) == 0 else "failure",
    "reason": reason,
    "transcript": lines,
}, indent=2) + "\n")
PY
  if mv "${RELEASE_DIR}" "${processed}" 2>/dev/null; then
    emit inbox INFO "archived to ${processed} (exit ${code})"
  else
    emit inbox WARN "could not archive ${RELEASE_DIR} to ${processed}; it will be re-attempted"
  fi
}

# -----------------------------------------------------------------------------
# Argument parsing.
# -----------------------------------------------------------------------------
parse_args() {
  while (($#)); do
    case "$1" in
      --execute) EXECUTE=true ;;
      --dry-run) EXECUTE=false ;;
      --inbox) INBOX_ROOT="${2:?--inbox needs a path}"; shift ;;
      --manifest) MANIFEST="${2:?--manifest needs a path}"; shift ;;
      --artifact-root) ARTIFACT_ROOT="${2:?--artifact-root needs a path}"; shift ;;
      --component) ONLY_COMPONENTS+=("${2:?--component needs a name}"); shift ;;
      --ack-migrations) ACK_MIGRATIONS=true ;;
      --announce-root) ANNOUNCE_ROOT="${2:?--announce-root needs an event id}"; shift ;;
      --relay-restart) RELAY_RESTART_MODE="${2:?--relay-restart needs a mode}"; shift ;;
      -h|--help) usage; exit 0 ;;
      -*) die_usage "unknown option $1" ;;
      *) [[ -z "${MANIFEST}" ]] || die_usage "unexpected argument $1"; MANIFEST="$1" ;;
    esac
    shift
  done
  if [[ -n "${INBOX_ROOT}" && -n "${MANIFEST}" ]]; then
    die_usage "--inbox and an explicit manifest are mutually exclusive"
  fi
  if [[ -z "${INBOX_ROOT}" && -z "${MANIFEST}" ]]; then
    die_usage "need a manifest path or --inbox <root>"
  fi
  if [[ -n "${ANNOUNCE_ROOT}" && ! "${ANNOUNCE_ROOT}" =~ ^[0-9a-f]{64}$ ]]; then
    die_usage "--announce-root must be a 64-char hex event id"
  fi
}

# -----------------------------------------------------------------------------
# Main.
# -----------------------------------------------------------------------------
main() {
  parse_args "$@"
  trap on_exit EXIT

  ${EXECUTE} || emit mode INFO "DRY RUN — nothing will be written, installed or restarted. Pass --execute to deploy."
  ${EXECUTE} && emit mode INFO "EXECUTE — this run will modify the live deployment."

  [[ -n "${INBOX_ROOT}" ]] && select_release
  [[ -f "${MANIFEST}" ]] || { emit preflight.manifest FAIL "no such manifest: ${MANIFEST}"; finish 2 "manifest missing"; }
  [[ -n "${ARTIFACT_ROOT}" ]] || ARTIFACT_ROOT="$(cd "$(dirname "${MANIFEST}")" && pwd)"

  load_manifest

  # ---- preflight: read-only, idempotent, and complete before anything moves.
  preflight_tooling
  preflight_layout
  preflight_source_commit
  preflight_no_fetch
  preflight_artifacts
  preflight_migrations
  preflight_disk
  preflight_stack
  gate_authorization

  if ${BLOCKED}; then
    # Only reachable in dry-run: refuse() exits immediately under --execute.
    finish 1 "dry run found ${FAIL_COUNT} blocker(s); nothing was mutated"
  fi

  announce "start" "Deploying \`${MF[name]}\` (\`${MF[source_commit]:0:12}\`) — components: ${ORDER[*]}, migrations: ${MF[migrations]}."

  # ---- backups, before the first byte moves.
  backup_current_state || deploy_failed "backup failed"
  full_backup || deploy_failed "full backup failed"

  # ---- components, in manifest order, one at a time.
  local comp
  for comp in "${ORDER[@]}"; do
    case "${comp}" in
      relay-image)
        set_relay_image "${MF[relay_image]}" "relay.image" || deploy_failed "could not set BUZZ_IMAGE"
        ${EXECUTE} && DID_SWAP_IMAGE=true
        restart_relay "relay.restart" || deploy_failed "relay restart failed"
        gate_relay "relay.gate" "${MF[relay_image]}" || deploy_failed "relay gate failed"
        announce "relay" "Deploy \`${MF[name]}\`: relay is healthy on \`${MF[relay_image]}\`."
        ;;
      acp)
        install_binary acp "acp.install" "${ARTIFACT_ROOT}/${MF[acp_artifact]}" \
          || deploy_failed "buzz-acp install failed"
        ${EXECUTE} && DID_INSTALL_ACP=true
        restart_agents || deploy_failed "agent restart/gate failed"
        ;;
      cli)
        install_binary cli "cli.install" "${ARTIFACT_ROOT}/${MF[cli_artifact]}" \
          || deploy_failed "buzz CLI install failed"
        ${EXECUTE} && DID_INSTALL_CLI=true
        gate_cli || deploy_failed "CLI smoke test failed"
        ;;
    esac
  done

  if ${EXECUTE}; then
    announce "result" "Deploy \`${MF[name]}\` (\`${MF[source_commit]:0:12}\`) succeeded. Components: ${ORDER[*]}. Backup: \`${BACKUP_DIR}\`."
    finish 0 "deployed ${MF[name]} @ ${MF[source_commit]:0:12}"
  fi
  finish 0 "dry run complete; ${PASS_COUNT} checks passed, nothing was mutated"
}

main "$@"
