#!/usr/bin/env bash
# =============================================================================
# render-updater-config.sh — turn the committed updater overlay into a
# buildable one by substituting the real public key.
# =============================================================================
#
# WHY THIS EXISTS
#
#   desktop/src-tauri/tauri.updater.conf.json is committed with the literal
#   placeholder `__BUZZ_UPDATER_PUBKEY__` where the minisign public key goes.
#   The repo is shared and mirrored; a key material file in it — even a public
#   one — invites the habit of putting the other half of the pair next to it.
#   So the repo carries the SHAPE of the config and this script carries the
#   VALUE, read from the box's key directory at build time.
#
#   The rendered file is deliberately written OUTSIDE the git worktree. Three
#   agents share that worktree and the orchestrator commits from it; a
#   generated config sitting untracked in src-tauri/ is a thing that eventually
#   gets `git add -A`-ed by accident.
#
# WHAT ELSE THE BUILD NEEDS (this is the part that surprises people)
#
#   The overlay is necessary but NOT sufficient. desktop/src-tauri/build.rs
#   emits `cargo:rustc-cfg=buzz_updater_enabled` only when BOTH
#   BUZZ_UPDATER_PUBLIC_KEY and BUZZ_UPDATER_ENDPOINT are set in the build
#   ENVIRONMENT, and desktop/src-tauri/src/lib.rs registers
#   tauri_plugin_updater ONLY under that cfg. Build with the overlay but
#   without the env vars and you get an app that has an endpoint in its config
#   and no updater plugin compiled in: every check fails with
#   "plugin updater not found" and the Settings panel reports
#   "Automatic updates aren't available on this build".
#
#   build.rs uses those two variables as a PRESENCE GATE only — it never
#   embeds their values. The endpoint and pubkey the running app actually uses
#   come from the merged Tauri config. They must still be non-empty, and
#   keeping them equal to the config values is the only way anyone reading the
#   build log can tell what was built.
#
#   `--print-env` emits exactly the exports the build needs, so the README's
#   build command is a copy-paste and not a reconstruction.
#
# USAGE
#
#   scripts/selfhost/desktop-updater/render-updater-config.sh [options]
#
#     --pubkey-file PATH   minisign public key file (default:
#                          /opt/buzz/keys/desktop-updater.key.pub)
#     --pubkey VALUE       use this base64 public key instead of a file
#     --endpoint URL       override the endpoint baked into the template
#     --out PATH           where to write the rendered config (default:
#                          /opt/buzz/build/desktop-updater/tauri.updater.conf.json)
#     --print-env          print `export` lines for the build environment
#     --quiet              only print the rendered config path
#
# EXIT CODES
#   0  rendered
#   1  refused (missing/invalid key, output would land in the worktree)
#   2  usage error
# =============================================================================
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

BUZZ_ROOT="${BUZZ_ROOT:-/opt/buzz}"
TEMPLATE="${BUZZ_UPDATER_TEMPLATE:-${REPO_ROOT}/desktop/src-tauri/tauri.updater.conf.json}"
PUBKEY_FILE="${BUZZ_UPDATER_PUBKEY_FILE:-${BUZZ_ROOT}/keys/desktop-updater.key.pub}"
OUT="${BUZZ_UPDATER_RENDERED_CONFIG:-${BUZZ_ROOT}/build/desktop-updater/tauri.updater.conf.json}"
PLACEHOLDER='__BUZZ_UPDATER_PUBKEY__'

PUBKEY=""
ENDPOINT_OVERRIDE=""
PRINT_ENV=false
QUIET=false

die() { printf 'render-updater-config: %s\n' "$*" >&2; exit "${2:-1}"; }
note() { $QUIET || printf 'render-updater-config: %s\n' "$*" >&2; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --pubkey-file) PUBKEY_FILE="${2:?--pubkey-file needs a path}"; shift 2 ;;
    --pubkey)      PUBKEY="${2:?--pubkey needs a value}"; shift 2 ;;
    --endpoint)    ENDPOINT_OVERRIDE="${2:?--endpoint needs a URL}"; shift 2 ;;
    --out)         OUT="${2:?--out needs a path}"; shift 2 ;;
    --print-env)   PRINT_ENV=true; shift ;;
    --quiet)       QUIET=true; shift ;;
    -h|--help)     sed -n '2,59p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *)             die "unknown argument: $1" 2 ;;
  esac
done

[[ -f "${TEMPLATE}" ]] || die "template not found: ${TEMPLATE}"

# ---------------------------------------------------------------------------
# The key.
#
# `tauri signer generate -w <keyfile>` writes <keyfile>.pub containing exactly
# the string Tauri expects in plugins.updater.pubkey: base64 of the minisign
# public key FILE (comment line included). tauri-plugin-updater's
# verify_signature() base64-decodes it and hands the result to
# minisign::PublicKey::decode, so anything that does not decode to a minisign
# public key file is a build that can never install an update — and it fails
# silently at update time, months later. Check it here instead.
# ---------------------------------------------------------------------------
if [[ -z "${PUBKEY}" ]]; then
  [[ -r "${PUBKEY_FILE}" ]] || die "public key not readable: ${PUBKEY_FILE} (run generate-keys.sh first, or pass --pubkey-file)"
  PUBKEY="$(tr -d '\r\n' <"${PUBKEY_FILE}")"
fi
[[ -n "${PUBKEY}" ]] || die "public key is empty"

decoded="$(printf '%s' "${PUBKEY}" | base64 -d 2>/dev/null || true)"
case "${decoded}" in
  "untrusted comment: minisign public key"*) : ;;
  *) die "public key does not base64-decode to a minisign public key file — is ${PUBKEY_FILE} the .pub file (not the private key)?" ;;
esac

# ---------------------------------------------------------------------------
# The output path. Never inside the worktree — see the header.
# ---------------------------------------------------------------------------
OUT_DIR="$(dirname "${OUT}")"
mkdir -p "${OUT_DIR}"
OUT_DIR_ABS="$(cd "${OUT_DIR}" && pwd)"
case "${OUT_DIR_ABS}/" in
  "${REPO_ROOT}/"*) die "refusing to write a rendered config inside the git worktree (${OUT}); pass --out somewhere under ${BUZZ_ROOT}" ;;
esac
OUT="${OUT_DIR_ABS}/$(basename "${OUT}")"

# ---------------------------------------------------------------------------
# Render. jq rather than sed so the key is inserted as a JSON string value and
# the result is proven to parse before anyone builds against it.
# ---------------------------------------------------------------------------
tmp="$(mktemp "${OUT}.XXXXXX")"
trap 'rm -f "${tmp}"' EXIT

jq --arg pubkey "${PUBKEY}" --arg placeholder "${PLACEHOLDER}" '
  if .plugins.updater.pubkey != $placeholder then
    error("template pubkey is not the placeholder \($placeholder) — refusing to overwrite a real value")
  else . end
  | .plugins.updater.pubkey = $pubkey
' "${TEMPLATE}" >"${tmp}"

if [[ -n "${ENDPOINT_OVERRIDE}" ]]; then
  jq --arg endpoint "${ENDPOINT_OVERRIDE}" '.plugins.updater.endpoints = [$endpoint]' "${tmp}" >"${tmp}.ep"
  mv "${tmp}.ep" "${tmp}"
fi

grep -Fq "${PLACEHOLDER}" "${tmp}" && die "placeholder survived substitution — template changed shape?"

ENDPOINT="$(jq -r '.plugins.updater.endpoints[0] // empty' "${tmp}")"
[[ -n "${ENDPOINT}" ]] || die "rendered config has no endpoint"
[[ "${ENDPOINT}" == https://* ]] || die "endpoint must be https (tauri-plugin-updater rejects other schemes in release builds): ${ENDPOINT}"
[[ "$(jq -r '.bundle.createUpdaterArtifacts' "${tmp}")" == "true" ]] || die "rendered config must set bundle.createUpdaterArtifacts=true"

chmod 0644 "${tmp}"
mv "${tmp}" "${OUT}"
trap - EXIT

note "endpoint  ${ENDPOINT}"
note "pubkey    ${PUBKEY:0:16}… (${#PUBKEY} chars)"
note "rendered  ${OUT}"

if $PRINT_ENV; then
  # These two are the presence gate build.rs looks for. Without them the
  # updater plugin is not compiled into the binary at all.
  printf 'export BUZZ_UPDATER_ENDPOINT=%q\n' "${ENDPOINT}"
  printf 'export BUZZ_UPDATER_PUBLIC_KEY=%q\n' "${PUBKEY}"
  printf 'export BUZZ_UPDATER_CONFIG=%q\n' "${OUT}"
else
  printf '%s\n' "${OUT}"
fi
