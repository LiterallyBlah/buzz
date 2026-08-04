# shellcheck shell=bash
# =============================================================================
# lib/waivers.sh — parse waivers.txt, classify test failures, shout about it.
# =============================================================================
# Policy this implements, from the release plan: GREEN MEANS GREEN. A waiver
# does not make a failure disappear; it makes the failure *loud and recorded*
# while letting the pipeline continue.
#
# Three outcomes for a test-suite gate:
#   * no failures                -> pass          (verdict stays `promotable`)
#   * all failures waived        -> pass, banner  (verdict -> `promotable_with_waivers`)
#   * any unwaived failure       -> fail          (verdict -> `blocked`)
#
# Requires lib/common.sh.
# =============================================================================

GATES_WAIVER_FILE="${GATES_WAIVER_FILE:-}"

# Populated by waivers_load / waivers_classify.
declare -a WAIVER_IDS=()
declare -a WAIVER_REASONS=()
declare -a WAIVER_DATES=()
declare -a WAIVERS_APPLIED=()   # "<test-id>" entries that actually matched a failure
declare -a WAIVERS_STALE=()     # waivers that matched nothing this run
declare -a FAILURES_UNWAIVED=()

waivers_load() {
  local file="$1"
  WAIVER_IDS=(); WAIVER_REASONS=(); WAIVER_DATES=()
  [[ -r "${file}" ]] || return 0
  local line id reason date
  while IFS= read -r line; do
    line="${line%%$'\r'}"
    [[ -z "${line//[[:space:]]/}" ]] && continue
    [[ "${line}" =~ ^[[:space:]]*# ]] && continue
    IFS='|' read -r id reason date <<< "${line}"
    id="$(echo "${id}" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    reason="$(echo "${reason:-}" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    date="$(echo "${date:-}" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    [[ -n "${id}" ]] || continue
    WAIVER_IDS+=("${id}")
    WAIVER_REASONS+=("${reason:-(no reason given — this is itself a defect)}")
    WAIVER_DATES+=("${date:-unknown}")
  done < "${file}"
}

# _waiver_index_for <failure-id> [binary]
# Matches a waiver against a failure. A waiver id containing `::` where the
# left side equals the reported binary is a scoped match; a bare id matches the
# test path in any binary. Echoes the array index, or -1.
_waiver_index_for() {
  local failure="$1" binary="${2:-}" i wid
  for i in "${!WAIVER_IDS[@]}"; do
    wid="${WAIVER_IDS[$i]}"
    if [[ "${wid}" == "${failure}" ]]; then echo "$i"; return 0; fi
    if [[ -n "${binary}" && "${wid}" == "${binary}::${failure}" ]]; then echo "$i"; return 0; fi
  done
  echo "-1"
}

# waivers_classify <failures_file>
# <failures_file> holds one failure per line as "<binary>\t<test-id>" (binary
# may be empty). Fills WAIVERS_APPLIED / WAIVERS_STALE / FAILURES_UNWAIVED.
# Returns 0 if nothing unwaived remains, 1 otherwise.
waivers_classify() {
  local failures_file="$1"
  WAIVERS_APPLIED=(); WAIVERS_STALE=(); FAILURES_UNWAIVED=()
  local -A hit=()
  local binary test idx

  if [[ -s "${failures_file}" ]]; then
    while IFS=$'\t' read -r binary test; do
      [[ -n "${test}" ]] || continue
      idx="$(_waiver_index_for "${test}" "${binary}")"
      if [[ "${idx}" == "-1" ]]; then
        FAILURES_UNWAIVED+=("${binary:+${binary}::}${test}")
      else
        hit["${idx}"]=1
        WAIVERS_APPLIED+=("${WAIVER_IDS[$idx]}")
      fi
    done < "${failures_file}"
  fi

  local i
  for i in "${!WAIVER_IDS[@]}"; do
    [[ -n "${hit[$i]:-}" ]] || WAIVERS_STALE+=("${WAIVER_IDS[$i]}")
  done

  [[ ${#FAILURES_UNWAIVED[@]} -eq 0 ]]
}

# waivers_report — the loud part. Called after every classify, unconditionally.
waivers_report() {
  local file="$1" i wid

  if [[ ${#WAIVERS_APPLIED[@]} -gt 0 ]]; then
    banner "${YELLOW}" \
      "WAIVERS APPLIED — THIS RUN IS NOT FULLY GREEN" \
      "" \
      "${#WAIVERS_APPLIED[@]} failing test(s) were waved through by:" \
      "${file}"
    for wid in "${WAIVERS_APPLIED[@]}"; do
      for i in "${!WAIVER_IDS[@]}"; do
        if [[ "${WAIVER_IDS[$i]}" == "${wid}" ]]; then
          echo -e "  ${YELLOW}${BOLD}✗ WAIVED${NC} ${wid}"
          echo -e "      ${DIM}added ${WAIVER_DATES[$i]} — ${WAIVER_REASONS[$i]}${NC}"
          break
        fi
      done
    done
    echo
    echo -e "  ${YELLOW}${BOLD}The goal state of ${file} is EMPTY.${NC}"
    echo -e "  ${YELLOW}Every line above is a debt. The stamp records it as${NC}"
    echo -e "  ${YELLOW}verdict=promotable_with_waivers, never plain 'promotable'.${NC}"
    echo
  fi

  if [[ ${#WAIVERS_STALE[@]} -gt 0 ]]; then
    warn "STALE waiver(s) — these matched no failure this run; delete them:"
    for wid in "${WAIVERS_STALE[@]}"; do
      echo -e "    ${YELLOW}·${NC} ${wid}"
    done
  fi

  if [[ ${#FAILURES_UNWAIVED[@]} -gt 0 ]]; then
    banner "${RED}" \
      "UNWAIVED TEST FAILURES — PROMOTION BLOCKED" \
      "" \
      "${#FAILURES_UNWAIVED[@]} failing test(s) have no waiver."
    for wid in "${FAILURES_UNWAIVED[@]}"; do
      echo -e "  ${RED}${BOLD}✗ FAIL${NC}   ${wid}"
    done
    echo
  fi
}

# waivers_json — the `waivers` block for promote-stamp.json.
waivers_json() {
  local file="$1"
  local declared='[]' applied='[]' stale='[]' unwaived='[]' i wid

  for i in "${!WAIVER_IDS[@]}"; do
    declared="$(jq -n --argjson acc "${declared}" \
      --arg id "${WAIVER_IDS[$i]}" --arg reason "${WAIVER_REASONS[$i]}" --arg date "${WAIVER_DATES[$i]}" \
      '$acc + [{test:$id, reason:$reason, added:$date}]')"
  done
  for wid in "${WAIVERS_APPLIED[@]:-}"; do
    [[ -n "${wid}" ]] || continue
    applied="$(jq -n --argjson acc "${applied}" --arg id "${wid}" '$acc + [$id]')"
  done
  for wid in "${WAIVERS_STALE[@]:-}"; do
    [[ -n "${wid}" ]] || continue
    stale="$(jq -n --argjson acc "${stale}" --arg id "${wid}" '$acc + [$id]')"
  done
  for wid in "${FAILURES_UNWAIVED[@]:-}"; do
    [[ -n "${wid}" ]] || continue
    unwaived="$(jq -n --argjson acc "${unwaived}" --arg id "${wid}" '$acc + [$id]')"
  done

  jq -n --arg file "${file}" \
        --argjson declared "${declared}" --argjson applied "${applied}" \
        --argjson stale "${stale}" --argjson unwaived "${unwaived}" \
    '{file:$file, declared:$declared, applied:$applied, stale:$stale,
      unwaived_failures:$unwaived}'
}

# ---- failure extraction -----------------------------------------------------

# cargo_failures <cargo_test_log> > <failures_file>
# Parses `cargo test` human output into "<binary>\t<test-id>" lines.
# Tracks the current test binary from the `Running ... (target/.../deps/<stem>-<hash>)`
# header so waivers can be scoped per binary.
cargo_failures() {
  local logfile="$1"
  awk '
    /^[[:space:]]*Running / {
      if (match($0, /deps\/[A-Za-z0-9_]+-[0-9a-f]+/)) {
        b = substr($0, RSTART + 5, RLENGTH - 5)
        sub(/-[0-9a-f]+$/, "", b)
        binary = b
      }
      infail = 0; next
    }
    /^[[:space:]]*Doc-tests /   { binary = "doctest"; infail = 0; next }
    /^failures:$/               { infail = 1; next }
    /^test result:/             { infail = 0; next }
    infail == 1 {
      line = $0
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)
      # `cargo test` prints TWO blocks headed "failures:": first the captured
      # stdout/panic detail for each failing test, then the bare summary list.
      # Only the summary list is wanted. Discriminate structurally rather than
      # by position: a Rust test path never contains whitespace and always
      # starts with an identifier character, whereas every line of panic detail
      # ("thread ... panicked at ...", "assertion failed: ...",
      # "---- name stdout ----", "note: run with RUST_BACKTRACE=1") contains a
      # space. Anything ambiguous falls through as a failure with no waiver,
      # which BLOCKS promotion — the safe direction to be wrong in.
      if (line == "") next
      if (line ~ /[[:space:]]/) next
      if (line !~ /^[A-Za-z_]/) next
      print binary "\t" line
    }
  ' "${logfile}" | sort -u
}

# node_failures <node_test_log> > <failures_file>
# node --test TAP output: failures are `not ok <n> - <name>`.
node_failures() {
  local logfile="$1"
  grep -E '^[[:space:]]*not ok [0-9]+ - ' "${logfile}" 2>/dev/null \
    | sed -E 's/^[[:space:]]*not ok [0-9]+ - //' \
    | sed -E 's/[[:space:]]*#.*$//' \
    | awk 'NF { print "desktop\t" $0 }' | sort -u || true
}
