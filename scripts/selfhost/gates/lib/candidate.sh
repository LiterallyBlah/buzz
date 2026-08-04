# shellcheck shell=bash
# =============================================================================
# lib/candidate.sh — identify and hash-bind the candidate under test.
# =============================================================================
# The point of a promotion stamp is that it says something about a SPECIFIC
# pile of bytes. This file turns "the candidate" into a lock: a list of
# (role, path, sha256, bytes) plus the source commit, taken BEFORE the gates
# run and re-taken at stamp time.
#
# Binding strength — deliberately two tiers, because this repo is a shared
# worktree with several agents editing concurrently:
#
#   HARD (a mismatch REFUSES the stamp):
#     * any locked artifact's sha256 changed
#     * any locked artifact disappeared
#     * HEAD moved to a different commit
#   These are the things that make the verdict a lie: they mean the bytes the
#   gates exercised are not the bytes the deployer would ship.
#
#   ADVISORY (recorded + warned, does not refuse):
#     * worktree dirtiness and the build-input tree digest
#   A source edit that never made it into a rebuild cannot invalidate a test
#   run of the old binary — but the deployer deserves to see it happened.
#
# Requires lib/common.sh to be sourced first.
# =============================================================================

# Map a cargo profile name to its target/ subdirectory. Cargo calls the
# development profile `dev` but writes to target/debug — same accommodation
# scripts/start-isolated-test-relay.sh:42-51 makes.
profile_target_dir() {
  case "$1" in
    dev|debug) echo "debug" ;;
    *)         echo "$1" ;;
  esac
}

# ---- staged artifacts -------------------------------------------------------
#
# GATES_ARTIFACT_DIR, when set, is a run-scoped directory holding COPIES of the
# built binaries. The lock hashes those copies and the gates execute those
# copies. This is not ceremony — it is what makes the hash binding mean
# anything, and it was added after the binding kept refusing valid runs:
#
#   cargo's feature unification is computed over the SELECTED package set. So
#   `cargo build -p buzz-relay -p buzz-acp -p buzz-cli` (the pre-lock build) and
#   `cargo test -p buzz-core -p buzz-sdk -p buzz-cli -p buzz-acp` (gate 1) each
#   rewrite target/<profile>/buzz-acp with DIFFERENT bytes, because buzz-relay
#   being in one set and not the other changes which features shared
#   dependencies are built with. Observed live: 79dadf79… before gate 1,
#   a93fc22c… after, with no source change and HEAD unmoved.
#
#   Hashing target/ directly therefore measures "what did cargo most recently
#   feel like emitting", not "what did we test". Staging a copy freezes the
#   candidate: nothing cargo does later can touch it, so any subsequent hash
#   change is genuine drift — which is exactly what the refusal is for.
#
# Unset GATES_ARTIFACT_DIR (e.g. `run-gates.sh lock` ad hoc) falls back to
# target/<profile>/, which is fine for inspection but is NOT a stable candidate.

# candidate_artifacts <repo_root> <profile>
# Emits "role<TAB>path_relative_to_root" for every artifact the gates exercise.
# Roles are the vocabulary the deployer's manifest should speak.
candidate_artifacts() {
  local root="$1" profile="$2" tdir base
  if [[ -n "${GATES_ARTIFACT_DIR:-}" ]]; then
    base="${GATES_ARTIFACT_DIR}"
  else
    tdir="$(profile_target_dir "${profile}")"
    base="${root}/target/${tdir}"
  fi
  printf 'relay\t%s/buzz-relay\n' "${base}"
  printf 'acp\t%s/buzz-acp\n' "${base}"
  printf 'cli\t%s/buzz\n' "${base}"
}

# candidate_bin <role> <repo_root> <profile> — absolute path to the binary the
# gates should EXECUTE. Always prefers the staged copy so the thing under test
# is the thing that was hashed.
candidate_bin() {
  local role="$1" root="$2" profile="$3" r p
  while IFS=$'\t' read -r r p; do
    [[ "${r}" == "${role}" ]] && { echo "${p}"; return 0; }
  done < <(candidate_artifacts "${root}" "${profile}")
  return 1
}

# candidate_stage <repo_root> <profile> <dest_dir>
# Copy the freshly built binaries into an immutable run-scoped directory.
candidate_stage() {
  local root="$1" profile="$2" dest="$3" tdir src
  tdir="$(profile_target_dir "${profile}")"
  mkdir -p "${dest}"
  for src in buzz-relay buzz-acp buzz; do
    if [[ -f "${root}/target/${tdir}/${src}" ]]; then
      cp -p "${root}/target/${tdir}/${src}" "${dest}/${src}" || return 1
    fi
  done
  chmod -w "${dest}"/* 2>/dev/null || true
  return 0
}

# Build-input digest. Scoped to the paths that can actually change the
# artifacts above, so it is fast and so unrelated churn from a sibling agent
# editing docs does not create noise. ADVISORY only — see header.
candidate_tree_digest() {
  local root="$1"
  ( cd "${root}" && git ls-files -z -- \
      Cargo.toml Cargo.lock rust-toolchain.toml crates desktop/src desktop/package.json \
      2>/dev/null \
    | xargs -0 -r sha256sum 2>/dev/null \
    | sha256sum | awk '{print $1}' )
}

# candidate_lock_json <repo_root> <profile>
# The canonical candidate description. Written once at run start
# (candidate-lock.json) and again at stamp time (candidate-verify.json).
candidate_lock_json() {
  local root="$1" profile="$2"
  local commit branch dirty tree_digest artifacts_json role rel abs

  commit="$(git -C "${root}" rev-parse HEAD 2>/dev/null || echo unknown)"
  branch="$(git -C "${root}" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
  if [[ -n "$(git -C "${root}" status --porcelain 2>/dev/null)" ]]; then dirty=true; else dirty=false; fi
  tree_digest="$(candidate_tree_digest "${root}")"

  artifacts_json='[]'
  while IFS=$'\t' read -r role rel; do
    [[ -n "${role}" ]] || continue
    abs="${rel}"
    artifacts_json="$(jq -n \
      --argjson acc "${artifacts_json}" \
      --arg role "${role}" \
      --arg path "${rel}" \
      --arg sha "$(sha256_file "${abs}")" \
      --argjson bytes "$(file_bytes "${abs}")" \
      --argjson present "$([[ -r "${abs}" ]] && echo true || echo false)" \
      '$acc + [{role:$role, path:$path, sha256:$sha, bytes:$bytes, present:$present}]')"
  done < <(candidate_artifacts "${root}" "${profile}")

  jq -n \
    --arg commit "${commit}" \
    --arg branch "${branch}" \
    --argjson dirty "${dirty}" \
    --arg tree_digest "${tree_digest}" \
    --arg profile "${profile}" \
    --arg taken_at "$(iso_now)" \
    --argjson artifacts "${artifacts_json}" \
    '{source_commit:$commit, source_branch:$branch, worktree_dirty:$dirty,
      build_input_digest:$tree_digest, cargo_profile:$profile,
      taken_at:$taken_at, artifacts:$artifacts}'
}

# baseline_json — the "deployed" side of the skew matrix, recorded for
# provenance so the deployer can confirm the skew gate compared against the
# artifacts that are actually live. Read-only: these paths are never written.
baseline_json() {
  local acp_path="${GATES_DEPLOYED_ACP:-/opt/buzz/bin/buzz-acp}"
  local image="${GATES_DEPLOYED_IMAGE:-buzz-local:unified-13acbaf2}"
  local image_id
  image_id="$(docker image inspect --format '{{.Id}}' "${image}" 2>/dev/null || echo "")"
  jq -n \
    --arg acp_path "${acp_path}" \
    --arg acp_sha "$(sha256_file "${acp_path}")" \
    --argjson acp_present "$([[ -r "${acp_path}" ]] && echo true || echo false)" \
    --arg image "${image}" \
    --arg image_id "${image_id}" \
    '{note:"the deployed side of the skew matrix; read-only inputs",
      artifacts:[
        {role:"acp", kind:"file",  ref:$acp_path, sha256:$acp_sha, present:$acp_present},
        {role:"relay", kind:"image", ref:$image, image_id:$image_id,
         present:($image_id|length > 0)}
      ]}'
}

# candidate_verify <lock_json_file> <verify_json_file>
# Emits a JSON drift report on stdout. Exit 0 = bound, 1 = drifted.
candidate_verify() {
  local lock="$1" verify="$2"
  jq -n --slurpfile a "${lock}" --slurpfile b "${verify}" '
    ($a[0]) as $lock | ($b[0]) as $now |
    {
      commit_changed: ($lock.source_commit != $now.source_commit),
      commit_before:  $lock.source_commit,
      commit_after:   $now.source_commit,
      tree_digest_changed: ($lock.build_input_digest != $now.build_input_digest),
      artifact_drift: [
        $lock.artifacts[] as $la
        | ($now.artifacts[] | select(.role == $la.role)) as $na
        | select($la.sha256 != $na.sha256 or ($la.present and ($na.present | not)))
        | {role: $la.role, path: $la.path,
           sha256_before: $la.sha256, sha256_after: $na.sha256,
           present_before: $la.present, present_after: $na.present}
      ]
    }
    | .bound = ((.commit_changed | not) and ((.artifact_drift | length) == 0))
  '
}
