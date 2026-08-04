# shellcheck shell=bash
# =============================================================================
# lib/common.sh — shared plumbing for the staging gate runner.
# =============================================================================
# Sourced by run-gates.sh and by every gate-*.sh. Never executed directly.
#
# Conventions this file establishes and every gate script relies on:
#   * DRY-RUN IS THE DEFAULT. `is_dry` is true unless the caller passed
#     --execute. Every side effect goes through `runx` (or is guarded by
#     `is_dry`) so `--dry-run` prints a complete, honest plan.
#   * Every gate writes exactly one machine-readable `result.json` into its
#     own evidence directory. stamp.sh reads those and nothing else — gates
#     never write the stamp themselves.
#   * Logging goes to stdout for humans; evidence goes to files. Never make
#     a verdict depend on scraping this script's own pretty output.
# =============================================================================

# Colour palette — matches scripts/run-tests.sh so gate output reads like the
# rest of the repo's tooling.
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
DIM='\033[2m'
BOLD='\033[1m'
NC='\033[0m'

# Honour NO_COLOR and non-tty output so evidence logs stay greppable.
if [[ -n "${NO_COLOR:-}" ]] || [[ ! -t 1 ]]; then
  RED='' GREEN='' YELLOW='' BLUE='' CYAN='' DIM='' BOLD='' NC=''
fi

GATE_TAG="${GATE_TAG:-gates}"

log()     { echo -e "${BLUE}[${GATE_TAG}]${NC} $*"; }
ok()      { echo -e "${GREEN}[${GATE_TAG}]${NC} $*"; }
warn()    { echo -e "${YELLOW}[${GATE_TAG}]${NC} $*"; }
err()     { echo -e "${RED}[${GATE_TAG}]${NC} $*" >&2; }
section() {
  echo
  echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
  echo -e "${CYAN}  $*${NC}"
  echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
}

_banner_line() {
  local colour="$1" text="$2"
  # Display width, not byte length. ${#x} is byte-oriented under a non-UTF-8
  # locale (which this host has), so an em-dash would count 3 and render the
  # box short. Stripping UTF-8 continuation bytes (0x80-0xBF) leaves exactly one
  # byte per character, which is the width we want. No locale assumptions, no
  # external tools.
  local ascii="${text//[$'\x80'-$'\xbf']/}"
  local pad=$(( 63 - ${#ascii} ))
  (( pad < 0 )) && pad=0
  printf "${colour}${BOLD}║${NC} %s%*s ${colour}${BOLD}║${NC}\n" "${text}" "${pad}" ""
}

# A banner nobody can scroll past. Used for waivers and for refused stamps.
banner() {
  local colour="$1"; shift
  local line
  echo
  echo -e "${colour}${BOLD}╔═════════════════════════════════════════════════════════════════╗${NC}"
  for line in "$@"; do
    # Never truncate: a banner exists to carry a message that must not be
    # missed, and a silently clipped reason is exactly the kind of "mostly
    # informative" output that lets a real failure slide. Long lines are
    # wrapped on word boundaries instead.
    # Pad on CHARACTER count, not byte count: printf's %-63s counts bytes, so a
    # line containing an em-dash or any other multibyte glyph would render the
    # box short and ragged. ${#line} is character-counted in a UTF-8 locale.
    if [[ ${#line} -le 63 ]]; then
      _banner_line "${colour}" "${line}"
    else
      local wrapped
      while IFS= read -r wrapped; do
        _banner_line "${colour}" "${wrapped}"
      done < <(printf '%s\n' "${line}" | fold -s -w 63)
    fi
  done
  echo -e "${colour}${BOLD}╚═════════════════════════════════════════════════════════════════╝${NC}"
  echo
}

# ---- dry-run machinery ------------------------------------------------------

# GATES_EXECUTE=1 only when the operator explicitly passed --execute.
is_dry() { [[ "${GATES_EXECUTE:-0}" != "1" ]]; }

_STEP_N=0

# step <label> — announce a numbered step. Always printed, in both modes, so a
# --dry-run transcript and an --execute transcript line up one-for-one.
step() {
  _STEP_N=$((_STEP_N + 1))
  printf '%b  %2d.%b %s\n' "${BOLD}" "${_STEP_N}" "${NC}" "$1"
}

# preview <argv...> — show the exact command line that would run / is running.
preview() {
  local rendered
  rendered="$(printf '%q ' "$@")"
  printf '      %b$ %s%b\n' "${DIM}" "${rendered% }" "${NC}"
}

# runx <label> -- <argv...>
#   dry-run : prints the step + exact argv, returns 0 without executing.
#   execute : prints the step + argv, then executes, propagating exit status.
# Use this for every command with a side effect. For loops/waits that cannot be
# a single argv, use `step` + an explicit `is_dry && return 0` guard.
runx() {
  local label="$1"; shift
  [[ "${1:-}" == "--" ]] && shift
  step "${label}"
  preview "$@"
  if is_dry; then return 0; fi
  "$@"
}

# note <text> — a dry-run-only explanation of a non-argv step (a wait loop, a
# parse, a marker assertion). Printed in both modes so plans stay honest.
note() { printf '      %b· %s%b\n' "${DIM}" "$1" "${NC}"; }

# ---- time / ids -------------------------------------------------------------

iso_now()  { date -u +%Y-%m-%dT%H:%M:%SZ; }
epoch_s()  { date -u +%s; }

# ---- hashing ----------------------------------------------------------------

# sha256_file <path> — bare hex digest, or empty string if unreadable. Never
# fails the caller: "unreadable" is a fact the stamp should record, not a crash.
sha256_file() {
  local p="$1"
  [[ -r "$p" ]] || { echo ""; return 0; }
  sha256sum -- "$p" 2>/dev/null | awk '{print $1}'
}

file_bytes() {
  local p="$1"
  [[ -r "$p" ]] || { echo "0"; return 0; }
  stat -c '%s' -- "$p" 2>/dev/null || echo "0"
}

# ---- JSON helpers -----------------------------------------------------------
# jq is a hard requirement: hand-rolled JSON in shell is how stamps end up
# malformed, and a malformed stamp is a deploy the deployer cannot verify.

require_jq() {
  command -v jq >/dev/null 2>&1 || {
    err "jq is required (stamp + gate results are JSON). Install jq."
    return 1
  }
}

# ---- gate result recording --------------------------------------------------

# record_result <evidence_dir> <name> <result> <started_epoch> <details_json>
#
# <result> ∈ pass | fail | skipped | dry-run | blocked
#   pass    — the gate ran and proved its claim
#   fail    — the gate ran and its claim was violated
#   skipped — deliberately not run (e.g. --only selected other gates)
#   dry-run — planned only; carries NO evidence of correctness
#   blocked — could not run for an environmental reason (recorded, not hidden)
#
# Writes <evidence_dir>/result.json. This file is the ONLY contract between a
# gate and stamp.sh.
record_result() {
  local dir="$1" name="$2" result="$3" started="$4" details="${5:-{\}}"
  local ended duration
  ended="$(epoch_s)"
  duration=$(( ended - started ))
  mkdir -p "${dir}"
  jq -n \
    --arg name "${name}" \
    --arg result "${result}" \
    --arg evidence "${dir}" \
    --argjson duration "${duration}" \
    --arg started_at "$(date -u -d "@${started}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || iso_now)" \
    --argjson details "${details}" \
    '{name:$name, result:$result, duration_s:$duration, started_at:$started_at,
      evidence:$evidence, details:$details}' \
    > "${dir}/result.json"
}

# print_result_line <name> <result> <duration> <evidence>
# The structured single-line PASS/FAIL output the task contract asks for.
print_result_line() {
  local name="$1" result="$2" duration="$3" evidence="$4"
  local colour tag
  case "${result}" in
    pass)    colour="${GREEN}";  tag="PASS" ;;
    fail)    colour="${RED}";    tag="FAIL" ;;
    blocked) colour="${RED}";    tag="BLOCKED" ;;
    dry-run) colour="${DIM}";    tag="DRY-RUN" ;;
    *)       colour="${YELLOW}"; tag="$(echo "${result}" | tr '[:lower:]' '[:upper:]')" ;;
  esac
  printf '%bGATE%b %-14s %b%-8s%b %6ss  %s\n' \
    "${BOLD}" "${NC}" "${name}" "${colour}${BOLD}" "${tag}" "${NC}" "${duration}" "${evidence}"
}

# ---- toolchain resolution ---------------------------------------------------

# The repo pins Rust 1.95.0 via rust-toolchain.toml. `cargo` is NOT on PATH in
# the agent environment; the rustup shim under ~/.cargo/bin honours the pin.
# Same reasoning as scripts/start-isolated-test-relay.sh:122-124.
ensure_cargo() {
  if ! command -v cargo >/dev/null 2>&1 && [[ -x "${HOME}/.cargo/bin/cargo" ]]; then
    export PATH="${HOME}/.cargo/bin:${PATH}"
  fi
  command -v cargo >/dev/null 2>&1 || { err "cargo not found (need Rust 1.95.0 per rust-toolchain.toml)"; return 1; }
}

# pnpm/node live outside the default PATH on the self-hosted box.
ensure_node() {
  local pnpm_dir="${GATES_PNPM_DIR:-${HOME}/.hermes/node/bin}"
  local node_dir="${GATES_NODE_DIR:-${HOME}/.local/bin}"
  [[ -d "${node_dir}" ]] && export PATH="${node_dir}:${PATH}"
  [[ -d "${pnpm_dir}" ]] && export PATH="${pnpm_dir}:${PATH}"
  command -v pnpm >/dev/null 2>&1 || { err "pnpm not found (looked in ${pnpm_dir})"; return 1; }
  command -v node >/dev/null 2>&1 || { err "node not found (looked in ${node_dir})"; return 1; }
}
