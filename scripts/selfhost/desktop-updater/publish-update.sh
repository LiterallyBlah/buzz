#!/usr/bin/env bash
# =============================================================================
# publish-update.sh — put a built desktop bundle on the self-hosted update
# channel, atomically.
# =============================================================================
#
# WHAT THIS PUBLISHES INTO
#
#   /opt/buzz/releases/desktop-updates/          ← served at /desktop-updates/
#   ├── latest.json                              ← MUTABLE. The only file the
#   │                                              app polls. Replaced by
#   │                                              rename(2), never edited.
#   └── artifacts/
#       ├── 0.5.5-unified.2/
#       │   ├── Buzz_0.5.5-unified.2_x64-setup.exe
#       │   ├── Buzz_0.5.5-unified.2_x64-setup.exe.sig
#       │   ├── latest.json          ← this version's manifest, verbatim.
#       │   │                          Rolling back is `cp` + `mv` of this file.
#       │   └── publish.json          ← audit record: sha256, size, source,
#       │                               who published it and when.
#       └── 0.5.6/…                   ← prior versions are NEVER removed.
#
# THE TWO PROPERTIES THAT MATTER
#
#   1. A POLLING CLIENT NEVER SEES A HALF-WRITTEN WORLD. The app checks
#      latest.json on launch and every six hours; there is no coordination
#      window. So: artifacts are staged in a sibling directory and moved into
#      place with rename(2) BEFORE latest.json is touched, and latest.json
#      itself is written to a temp file and renamed over the old one. rename(2)
#      within a filesystem is atomic — a reader gets the whole old file or the
#      whole new one. Nothing here ever writes to a path a client can read.
#
#   2. A PUBLISHED VERSION IS WRITE-ONCE. artifacts/<version>/ is refused if it
#      already exists. The signature in latest.json is bound to exact bytes; if
#      those bytes can be replaced under a URL that some client has already
#      resolved, the manifest is describing a file that no longer exists. Use
#      a new version number. `--force` exists for the case where you are
#      certain the old bytes were never fetched; it moves the old directory to
#      artifacts/.superseded/ rather than deleting it.
#
# WHY A DIRECTORY MOUNT AND NOT THE RELAY MEDIA STORE
#
#   The relay's media store is hash-addressed and immutable, which is the right
#   shape for an artifact — but it denies `.exe`, and the Windows updater
#   artifact IS an `.exe` (Tauri signs the NSIS installer in place). Wrapping
#   it in an allowed `.zip` would invalidate the `.sig`, which covers the exe's
#   bytes, so it would have to be re-signed as a zip — a second signing step,
#   on a second artifact, with the same key, for no gain. The write-once
#   artifacts/<version>/ layout above gives the same immutability with one
#   fewer moving part. See README.md.
#
# USAGE
#
#   scripts/selfhost/desktop-updater/publish-update.sh \
#       --bundle-dir desktop/src-tauri/target/x86_64-pc-windows-msvc/release/bundle \
#       --version 0.5.5-unified.2 \
#       --notes "Fixes the thing." \
#       [--execute]
#
#     --bundle-dir DIR    tauri build output dir (searched recursively for *.sig)
#     --version V         semver; must sort ABOVE the currently published one
#     --notes TEXT        release notes shown in the app
#     --notes-file PATH   … or read them from a file
#     --execute           actually publish (DRY RUN IS THE DEFAULT)
#     --root PATH         publish root (default /opt/buzz/releases/desktop-updates)
#     --base-url URL      URL the root is served at
#     --platform KEY      updater platform key (default windows-x86_64)
#     --force             replace an already-published version (see above)
#
# OUTPUT CONTRACT
#
#   buzz-updater ts=<iso8601> step=<name> status=<PASS|FAIL|SKIP|PLAN|INFO|WARN> <detail>
#
# EXIT CODES
#   0  published (or a dry run with no blockers)
#   1  refused, or a dry run found blockers — nothing was mutated
#   2  usage error — nothing was mutated
#   3  publish failed after mutation and rolled back cleanly
#   4  publish failed AND ROLLBACK FAILED — a human must intervene now
# =============================================================================
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BUZZ_ROOT="${BUZZ_ROOT:-/opt/buzz}"
PUBLISH_ROOT="${BUZZ_UPDATES_ROOT:-${BUZZ_ROOT}/releases/desktop-updates}"
BASE_URL="${BUZZ_UPDATES_BASE_URL:-https://hermes.tail81f3.ts.net:9443/desktop-updates}"
PLATFORM_KEY="${BUZZ_UPDATES_PLATFORM:-windows-x86_64}"

BUNDLE_DIR=""
VERSION=""
NOTES=""
NOTES_SET=false
EXECUTE=false
FORCE=false

PASS_COUNT=0
FAIL_COUNT=0
BLOCKED=false
STAGING_DIR=""
VERSION_DIR=""
LATEST_BACKUP=""
DID_INSTALL_VERSION=false
DID_SWAP_LATEST=false
SUPERSEDED_FROM=""
SUPERSEDED_TO=""

# ---------------------------------------------------------------------------
# Output. Same contract as scripts/selfhost/deploy.sh: everything on stdout,
# top to bottom, so the transcript reads as what happened.
# ---------------------------------------------------------------------------
now() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }
emit() { printf 'buzz-updater ts=%s step=%s status=%s %s\n' "$(now)" "$1" "$2" "${3-}"; }
step_pass() { PASS_COUNT=$((PASS_COUNT + 1)); emit "$1" PASS "${2-}"; }
step_fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); emit "$1" FAIL "${2-}"; }
step_info() { emit "$1" INFO "${2-}"; }
step_warn() { emit "$1" WARN "${2-}"; }
step_plan() { emit "$1" PLAN "${2-}"; }
step_skip() { emit "$1" SKIP "${2-}"; }

usage_error() { emit usage FAIL "$*"; exit 2; }
block() { step_fail "$1" "${2-}"; BLOCKED=true; }

# ---------------------------------------------------------------------------
# Arguments.
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bundle-dir) BUNDLE_DIR="${2:?--bundle-dir needs a path}"; shift 2 ;;
    --version)    VERSION="${2:?--version needs a value}"; shift 2 ;;
    --notes)      NOTES="${2?--notes needs a value}"; NOTES_SET=true; shift 2 ;;
    --notes-file) NOTES="$(cat "${2:?--notes-file needs a path}")"; NOTES_SET=true; shift 2 ;;
    --execute)    EXECUTE=true; shift ;;
    --root)       PUBLISH_ROOT="${2:?--root needs a path}"; shift 2 ;;
    --base-url)   BASE_URL="${2:?--base-url needs a URL}"; shift 2 ;;
    --platform)   PLATFORM_KEY="${2:?--platform needs a key}"; shift 2 ;;
    --force)      FORCE=true; shift ;;
    -h|--help)    sed -n '2,80p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *)            usage_error "unknown argument: $1" ;;
  esac
done

[[ -n "${BUNDLE_DIR}" ]] || usage_error "--bundle-dir is required"
[[ -n "${VERSION}" ]] || usage_error "--version is required"
$NOTES_SET || usage_error "--notes or --notes-file is required (the app shows these to the user; an auto-generated string is not release notes)"
[[ -n "${NOTES}" ]] || usage_error "release notes are empty"
BASE_URL="${BASE_URL%/}"

command -v jq >/dev/null || usage_error "jq is required"
command -v python3 >/dev/null || usage_error "python3 is required"

step_info run "version=${VERSION} platform=${PLATFORM_KEY} root=${PUBLISH_ROOT} mode=$($EXECUTE && echo execute || echo dry-run)"

# ---------------------------------------------------------------------------
# Semver precedence, matching `semver::Version` — which is what
# tauri-plugin-updater compares with (`release.version > self.current_version`
# in updater.rs). Prerelease rules are the whole point: 0.5.5-unified.1 is
# LOWER than 0.5.5, and 0.5.5-unified.2 is higher than 0.5.5-unified.1. Getting
# this wrong ships an "update" the client silently declines.
# ---------------------------------------------------------------------------
semver_gt() {
  python3 - "$1" "$2" <<'PY'
import re, sys

PATTERN = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)


def key(raw):
    m = PATTERN.match(raw)
    if not m:
        sys.exit(2)
    major, minor, patch, pre, _build = m.groups()
    # Absent prerelease outranks any present one.
    ids = []
    if pre is not None:
        for part in pre.split("."):
            # Numeric identifiers compare numerically and rank below alphanumeric.
            ids.append((0, int(part), "") if part.isdigit() else (1, 0, part))
    return (int(major), int(minor), int(patch), 1 if pre is None else 0, ids)


sys.exit(0 if key(sys.argv[1]) > key(sys.argv[2]) else 1)
PY
}

semver_valid() {
  [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]
}

# ---------------------------------------------------------------------------
# Preflight. Nothing below this block mutates anything.
# ---------------------------------------------------------------------------
if semver_valid "${VERSION}"; then
  step_pass preflight.version "${VERSION}"
else
  block preflight.version "not semver (expected 1.2.3 or 1.2.3-tag.1): ${VERSION}"
fi

if [[ -d "${BUNDLE_DIR}" ]]; then
  BUNDLE_DIR="$(cd "${BUNDLE_DIR}" && pwd)"
  step_pass preflight.bundle-dir "${BUNDLE_DIR}"
else
  block preflight.bundle-dir "not a directory: ${BUNDLE_DIR}"
fi

# The updater artifact is identified by its SIGNATURE, not by its extension.
# `createUpdaterArtifacts: true` emits exactly one `<artifact>.sig` per bundle
# target, so "find the .sig, the artifact is its stem" is the one rule that
# holds across NSIS, MSI, tar.gz and AppImage without special-casing any of them.
ARTIFACT=""
SIGFILE=""
if [[ -d "${BUNDLE_DIR}" ]]; then
  mapfile -t sigs < <(find "${BUNDLE_DIR}" -type f -name '*.sig' | sort)
  case "${#sigs[@]}" in
    0) block preflight.signature "no *.sig under ${BUNDLE_DIR} — the build did not run with bundle.createUpdaterArtifacts=true (use tauri.updater.conf.json) or TAURI_SIGNING_PRIVATE_KEY was unset" ;;
    1) SIGFILE="${sigs[0]}"; ARTIFACT="${SIGFILE%.sig}"; step_pass preflight.signature "${SIGFILE}" ;;
    *) block preflight.signature "${#sigs[@]} signature files under ${BUNDLE_DIR}; clean the bundle dir or point --bundle-dir at one target: ${sigs[*]}" ;;
  esac
fi

SIGNATURE=""
ARTIFACT_NAME=""
ARTIFACT_SHA=""
ARTIFACT_SIZE=""
if [[ -n "${ARTIFACT}" ]]; then
  if [[ -f "${ARTIFACT}" ]]; then
    ARTIFACT_NAME="$(basename "${ARTIFACT}")"
    ARTIFACT_SHA="$(sha256sum "${ARTIFACT}" | cut -d' ' -f1)"
    ARTIFACT_SIZE="$(stat -c '%s' "${ARTIFACT}")"
    step_pass preflight.artifact "${ARTIFACT_NAME} bytes=${ARTIFACT_SIZE} sha256=${ARTIFACT_SHA}"
  else
    block preflight.artifact "signature ${SIGFILE} has no matching artifact at ${ARTIFACT}"
  fi
fi

# The .sig is base64 of a minisign signature file whose trusted comment names
# the file it covers. Checking that name against the artifact catches the
# classic mistake — a signature left over from the previous build — which
# otherwise surfaces as a signature failure on the user's machine, after the
# download, with no useful message.
if [[ -n "${SIGFILE}" && -f "${SIGFILE}" ]]; then
  SIGNATURE="$(tr -d '\r\n' <"${SIGFILE}")"
  if [[ -z "${SIGNATURE}" ]]; then
    block preflight.signature-content "signature file is empty: ${SIGFILE}"
  else
    decoded="$(printf '%s' "${SIGNATURE}" | base64 -d 2>/dev/null || true)"
    if [[ "${decoded}" != *"untrusted comment:"* ]]; then
      block preflight.signature-content "signature does not base64-decode to a minisign signature file: ${SIGFILE}"
    else
      signed_name="$(printf '%s' "${decoded}" | sed -n 's/.*[[:space:]]file:\([^[:space:]]*\).*/\1/p' | head -1)"
      if [[ -n "${signed_name}" && -n "${ARTIFACT_NAME}" && "${signed_name}" != "${ARTIFACT_NAME}" ]]; then
        block preflight.signature-content "signature covers '${signed_name}' but the artifact is '${ARTIFACT_NAME}' — stale .sig from an earlier build"
      else
        step_pass preflight.signature-content "minisign signature over ${signed_name:-${ARTIFACT_NAME}}"
      fi
    fi
  fi
fi

# A version string that does not appear in the artifact name is almost always
# a typo in --version, and it produces an update the client accepts and then
# installs as the wrong build. Warn rather than block: bundle naming is a
# bundler detail and not a contract.
if [[ -n "${ARTIFACT_NAME}" && "${ARTIFACT_NAME}" != *"${VERSION}"* ]]; then
  step_warn preflight.version-in-name "artifact '${ARTIFACT_NAME}' does not contain '${VERSION}' — check --version"
fi

CURRENT_VERSION=""
LATEST_JSON="${PUBLISH_ROOT}/latest.json"
if [[ -f "${LATEST_JSON}" ]]; then
  CURRENT_VERSION="$(jq -r '.version // empty' "${LATEST_JSON}" 2>/dev/null || true)"
  if [[ -z "${CURRENT_VERSION}" ]]; then
    step_warn preflight.current "existing ${LATEST_JSON} has no .version — it will be replaced"
  elif semver_gt "${VERSION}" "${CURRENT_VERSION}"; then
    step_pass preflight.ordering "${VERSION} > ${CURRENT_VERSION} (published)"
  else
    block preflight.ordering "${VERSION} does not sort above the published ${CURRENT_VERSION}; tauri-plugin-updater compares with semver precedence and would decline this as not-an-update"
  fi
else
  step_info preflight.current "no latest.json yet — this is the first publish"
fi

VERSION_DIR="${PUBLISH_ROOT}/artifacts/${VERSION}"
if [[ -e "${VERSION_DIR}" ]]; then
  if $FORCE; then
    step_warn preflight.write-once "${VERSION_DIR} exists; --force will move it to artifacts/.superseded/"
  else
    block preflight.write-once "${VERSION_DIR} already exists — a published version is write-once. Bump the version, or pass --force if you are certain nothing ever fetched those bytes."
  fi
else
  step_pass preflight.write-once "${VERSION_DIR} is free"
fi

if $EXECUTE; then
  # 0755 explicitly, not whatever umask the caller happens to have: tailscaled
  # serves these files as root, but a 0700 directory here is a 403 for every
  # client and the symptom (checks succeed, downloads fail) is a bad one to
  # debug from the Windows side.
  if mkdir -p "${PUBLISH_ROOT}/artifacts" 2>/dev/null &&
     chmod 0755 "${PUBLISH_ROOT}" "${PUBLISH_ROOT}/artifacts" 2>/dev/null &&
     [[ -w "${PUBLISH_ROOT}/artifacts" ]]; then
    step_pass preflight.writable "${PUBLISH_ROOT}"
  else
    block preflight.writable "cannot write ${PUBLISH_ROOT}/artifacts (run as the owner of ${PUBLISH_ROOT}, or with sudo)"
  fi
else
  step_skip preflight.writable "dry run does not create ${PUBLISH_ROOT}"
fi

URL="${BASE_URL}/artifacts/${VERSION}/${ARTIFACT_NAME}"
PUB_DATE="$(now)"

# Build the manifest now so a dry run shows the exact bytes that would ship.
MANIFEST="$(jq -n \
  --arg version "${VERSION}" \
  --arg notes "${NOTES}" \
  --arg pub_date "${PUB_DATE}" \
  --arg platform "${PLATFORM_KEY}" \
  --arg signature "${SIGNATURE}" \
  --arg url "${URL}" \
  '{version: $version, notes: $notes, pub_date: $pub_date,
    platforms: {($platform): {signature: $signature, url: $url}}}')"

step_plan manifest.url "${URL}"
step_plan manifest.bytes "$(printf '%s' "${MANIFEST}" | jq -c '.platforms |= map_values(.signature |= (.[0:12] + "…"))')"

if $BLOCKED; then
  emit result FAIL "refused: ${FAIL_COUNT} blocker(s); nothing was mutated"
  exit 1
fi

if ! $EXECUTE; then
  emit result PASS "dry run clean (${PASS_COUNT} checks). Re-run with --execute to publish."
  exit 0
fi

# ---------------------------------------------------------------------------
# Mutation. Everything below can fail; the rollback below undoes exactly what
# this run did and nothing else.
# ---------------------------------------------------------------------------
rollback() {
  local rc=$1
  emit rollback INFO "undoing this run"
  local ok=true
  if $DID_SWAP_LATEST; then
    if [[ -n "${LATEST_BACKUP}" && -f "${LATEST_BACKUP}" ]]; then
      mv -f "${LATEST_BACKUP}" "${LATEST_JSON}" && step_pass rollback.latest "restored previous latest.json" || { step_fail rollback.latest "could not restore ${LATEST_JSON} from ${LATEST_BACKUP}"; ok=false; }
    else
      rm -f "${LATEST_JSON}" && step_pass rollback.latest "removed the latest.json this run created" || { step_fail rollback.latest "could not remove ${LATEST_JSON}"; ok=false; }
    fi
  fi
  if $DID_INSTALL_VERSION; then
    rm -rf "${VERSION_DIR}" && step_pass rollback.artifacts "removed ${VERSION_DIR}" || { step_fail rollback.artifacts "could not remove ${VERSION_DIR}"; ok=false; }
    if [[ -n "${SUPERSEDED_TO}" && -d "${SUPERSEDED_TO}" ]]; then
      mv "${SUPERSEDED_TO}" "${SUPERSEDED_FROM}" && step_pass rollback.superseded "restored ${SUPERSEDED_FROM}" || { step_fail rollback.superseded "could not restore ${SUPERSEDED_FROM} from ${SUPERSEDED_TO}"; ok=false; }
    fi
  fi
  [[ -n "${STAGING_DIR}" && -d "${STAGING_DIR}" ]] && rm -rf "${STAGING_DIR}" || true
  [[ -n "${LATEST_BACKUP}" && -f "${LATEST_BACKUP}" ]] && rm -f "${LATEST_BACKUP}" || true
  if $ok; then
    emit result FAIL "publish failed and rolled back cleanly"
    exit "${rc}"
  fi
  emit result FAIL "PUBLISH FAILED AND ROLLBACK FAILED — ${PUBLISH_ROOT} needs a human now"
  exit 4
}
trap 'rollback 3' ERR

# Staging lives inside the publish root so the final move is a rename(2) on the
# same filesystem — a cross-device mv would be a copy, and a copy into a served
# directory is exactly the half-written state this script exists to avoid.
STAGING_DIR="${PUBLISH_ROOT}/artifacts/.staging.${VERSION}.$$"
rm -rf "${STAGING_DIR}"
mkdir -p "${STAGING_DIR}"
chmod 0755 "${STAGING_DIR}"
step_pass stage.mkdir "${STAGING_DIR}"

cp -p "${ARTIFACT}" "${STAGING_DIR}/${ARTIFACT_NAME}"
cp -p "${SIGFILE}" "${STAGING_DIR}/${ARTIFACT_NAME}.sig"
chmod 0644 "${STAGING_DIR}/${ARTIFACT_NAME}" "${STAGING_DIR}/${ARTIFACT_NAME}.sig"

staged_sha="$(sha256sum "${STAGING_DIR}/${ARTIFACT_NAME}" | cut -d' ' -f1)"
[[ "${staged_sha}" == "${ARTIFACT_SHA}" ]] || { step_fail stage.copy "sha256 changed in transit: ${staged_sha} != ${ARTIFACT_SHA}"; false; }
step_pass stage.copy "${ARTIFACT_NAME} sha256=${staged_sha}"

printf '%s\n' "${MANIFEST}" >"${STAGING_DIR}/latest.json"
jq -n \
  --arg version "${VERSION}" \
  --arg artifact "${ARTIFACT_NAME}" \
  --arg sha256 "${ARTIFACT_SHA}" \
  --argjson size "${ARTIFACT_SIZE}" \
  --arg platform "${PLATFORM_KEY}" \
  --arg url "${URL}" \
  --arg source "${ARTIFACT}" \
  --arg published_at "${PUB_DATE}" \
  --arg published_by "$(id -un)@$(hostname)" \
  '{version: $version, platform: $platform, artifact: $artifact, sha256: $sha256,
    size: $size, url: $url, source: $source,
    published_at: $published_at, published_by: $published_by}' \
  >"${STAGING_DIR}/publish.json"
chmod 0644 "${STAGING_DIR}/latest.json" "${STAGING_DIR}/publish.json"
step_pass stage.manifest "${STAGING_DIR}/latest.json"

# Supersede an existing version dir only under --force, and by moving it aside
# rather than deleting it. Preflight already refused the non-force case.
if [[ -e "${VERSION_DIR}" ]]; then
  SUPERSEDED_FROM="${VERSION_DIR}"
  SUPERSEDED_TO="${PUBLISH_ROOT}/artifacts/.superseded/${VERSION}.$(date -u +%Y%m%dT%H%M%SZ)"
  mkdir -p "$(dirname "${SUPERSEDED_TO}")"
  mv "${VERSION_DIR}" "${SUPERSEDED_TO}"
  step_warn publish.supersede "moved previous ${VERSION} to ${SUPERSEDED_TO}"
fi

# STEP 1 of 2: artifacts land first. A client that reads the old latest.json in
# this instant is unaffected; the new directory is simply not referenced yet.
mv "${STAGING_DIR}" "${VERSION_DIR}"
DID_INSTALL_VERSION=true
STAGING_DIR=""
step_pass publish.artifacts "${VERSION_DIR}"

# STEP 2 of 2: swap the pointer. Write beside, then rename over — rename(2) is
# atomic within a filesystem, so a poller sees the whole old manifest or the
# whole new one and never a truncated read.
if [[ -f "${LATEST_JSON}" ]]; then
  LATEST_BACKUP="${PUBLISH_ROOT}/.latest.json.prev.$$"
  cp -p "${LATEST_JSON}" "${LATEST_BACKUP}"
fi
tmp_latest="${PUBLISH_ROOT}/.latest.json.new.$$"
cp "${VERSION_DIR}/latest.json" "${tmp_latest}"
chmod 0644 "${tmp_latest}"
mv -f "${tmp_latest}" "${LATEST_JSON}"
DID_SWAP_LATEST=true
step_pass publish.latest "${LATEST_JSON}"

# ---------------------------------------------------------------------------
# Verify after. Re-read what is actually on disk — not what we believe we
# wrote — and check that the manifest describes a file that exists with the
# bytes the signature covers.
# ---------------------------------------------------------------------------
jq -e . "${LATEST_JSON}" >/dev/null || { step_fail verify.parse "published latest.json does not parse"; false; }
published_version="$(jq -r '.version' "${LATEST_JSON}")"
[[ "${published_version}" == "${VERSION}" ]] || { step_fail verify.version "latest.json says ${published_version}"; false; }
published_url="$(jq -r --arg p "${PLATFORM_KEY}" '.platforms[$p].url' "${LATEST_JSON}")"
published_sig="$(jq -r --arg p "${PLATFORM_KEY}" '.platforms[$p].signature' "${LATEST_JSON}")"
[[ "${published_url}" == "${URL}" ]] || { step_fail verify.url "latest.json url is ${published_url}"; false; }
[[ "${published_sig}" == "${SIGNATURE}" ]] || { step_fail verify.signature "signature in latest.json does not match ${SIGFILE}"; false; }

# The URL must resolve to a real local path under the publish root. This is the
# check that catches a --base-url that does not describe how the directory is
# actually served.
url_prefix="${BASE_URL}/"
rel="${published_url#"${url_prefix}"}"
[[ "${rel}" != "${published_url}" ]] || { step_fail verify.base-url "published url ${published_url} is not under --base-url ${BASE_URL}"; false; }
served_path="${PUBLISH_ROOT}/${rel}"
[[ -f "${served_path}" ]] || { step_fail verify.served "manifest points at ${published_url} but ${served_path} does not exist"; false; }
served_sha="$(sha256sum "${served_path}" | cut -d' ' -f1)"
[[ "${served_sha}" == "${ARTIFACT_SHA}" ]] || { step_fail verify.served "${served_path} sha256=${served_sha} != ${ARTIFACT_SHA}"; false; }
step_pass verify.served "${published_url} -> ${served_path} sha256=${served_sha}"

trap - ERR
[[ -n "${LATEST_BACKUP}" && -f "${LATEST_BACKUP}" ]] && rm -f "${LATEST_BACKUP}" || true

kept="$(find "${PUBLISH_ROOT}/artifacts" -mindepth 1 -maxdepth 1 -type d ! -name '.*' -printf '%f\n' | sort | tr '\n' ' ')"
step_info publish.retained "versions on disk: ${kept}"
step_info publish.next "clients pick this up on next launch or within 6h; in-app: Settings -> Software Updates -> Check for Updates"
emit result PASS "published ${VERSION} (${PASS_COUNT} checks)"
