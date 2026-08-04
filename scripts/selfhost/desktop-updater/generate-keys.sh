#!/usr/bin/env bash
# =============================================================================
# generate-keys.sh — mint the box's desktop updater signing key. ONCE.
# =============================================================================
#
# This is a thin wrapper around `pnpm tauri signer generate -w <keyfile>`.
# The wrapper exists for four reasons, all of them about the failure modes of
# the bare command rather than the command itself:
#
#   1. IT REFUSES TO OVERWRITE. The bare command has a `--force` flag and will
#      happily replace an existing key. If that ever happens on this box every
#      installed copy of Buzz stops accepting updates — permanently, because
#      the old public key is compiled into the installed binary and there is no
#      channel left to ship a new one through. Losing this key is the one
#      unrecoverable state in the whole updater design, so the wrapper's first
#      and most important job is to not do that.
#
#   2. IT FIXES THE FILE MODE. `tauri signer generate` writes the private key
#      0644. This script runs the generator under `umask 077` so the key is
#      never world-readable, not even for the instant between write and chmod.
#
#   3. IT NEVER PRINTS THE PRIVATE KEY. The generator does not print it either
#      (verified against tauri-cli 2.x), but "verified once" is not a property
#      — so the wrapper greps its own captured output for the key bytes and
#      refuses to relay anything it finds.
#
#   4. IT SAYS WHAT TO DO NEXT. The key is only useful in combination with two
#      environment variables at build time and a public half pasted into a
#      config. Those instructions belong at the moment of generation, not in a
#      document someone reads six months later.
#
# WHERE THE KEY LIVES
#
#   Default: /opt/buzz/keys/desktop-updater.key  (private, 0600)
#            /opt/buzz/keys/desktop-updater.key.pub  (public, 0644)
#
#   The private key should be owned root:root inside a root-owned 0700
#   directory. Mode 0600 alone is not enough: on a directory the build user can
#   write, that user cannot READ the key but can still DELETE and REPLACE it,
#   which is the same attack with an extra step. This script checks and prints
#   the exact commands when the invariant does not hold.
#
#   BACK IT UP OFF THE BOX. Encrypted, offline, today. See the README.
#
# HOW THE BUILD CONSUMES IT
#
#   TAURI_SIGNING_PRIVATE_KEY           the CONTENTS of the private key file
#   TAURI_SIGNING_PRIVATE_KEY_PASSWORD  the password chosen below (empty if none)
#
#   e.g.  export TAURI_SIGNING_PRIVATE_KEY="$(sudo cat /opt/buzz/keys/desktop-updater.key)"
#         export TAURI_SIGNING_PRIVATE_KEY_PASSWORD='…'
#
#   (tauri-cli also accepts TAURI_SIGNING_PRIVATE_KEY_PATH as a path-valued
#   alternative to the first one. Either works; the README uses the contents
#   form because it is the one the OSS release workflow uses.)
#
# USAGE
#
#   sudo scripts/selfhost/desktop-updater/generate-keys.sh [options]
#
#     --keyfile PATH   where to write the private key
#                      (default /opt/buzz/keys/desktop-updater.key)
#     --password PW    non-interactive password (otherwise the generator prompts)
#     --no-password    generate an unprotected key — allowed, but say it out loud
#
# EXIT CODES
#   0  key generated
#   1  refused (key exists, tooling missing, generator leaked the key)
#   2  usage error
# =============================================================================
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
DESKTOP_DIR="${REPO_ROOT}/desktop"

BUZZ_ROOT="${BUZZ_ROOT:-/opt/buzz}"
KEYFILE="${BUZZ_UPDATER_KEYFILE:-${BUZZ_ROOT}/keys/desktop-updater.key}"
PASSWORD=""
PASSWORD_SET=false
NO_PASSWORD=false

die() { printf 'generate-keys: %s\n' "$*" >&2; exit "${2:-1}"; }
say() { printf 'generate-keys: %s\n' "$*"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --keyfile)     KEYFILE="${2:?--keyfile needs a path}"; shift 2 ;;
    --password)    PASSWORD="${2:?--password needs a value}"; PASSWORD_SET=true; shift 2 ;;
    --no-password) NO_PASSWORD=true; shift ;;
    -h|--help)     sed -n '2,70p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *)             die "unknown argument: $1" 2 ;;
  esac
done

$PASSWORD_SET && $NO_PASSWORD && die "--password and --no-password are mutually exclusive" 2

PUBFILE="${KEYFILE}.pub"

# ---------------------------------------------------------------------------
# Rule 1: never overwrite. Both halves are checked — a stray .pub with no
# private half means a previous run was interrupted and a human must look at
# it, not that it is safe to generate over the top.
# ---------------------------------------------------------------------------
[[ -e "${KEYFILE}" ]] && die "REFUSING: ${KEYFILE} already exists. This box already has an updater key; regenerating it would permanently orphan every installed copy of Buzz. If you truly mean to rotate, read the rotation section of the README first."
[[ -e "${PUBFILE}" ]] && die "REFUSING: ${PUBFILE} exists but ${KEYFILE} does not. A previous run was interrupted or the private half was moved. Resolve by hand."

KEYDIR="$(dirname "${KEYFILE}")"
mkdir -p "${KEYDIR}"
chmod 0700 "${KEYDIR}"

# ---------------------------------------------------------------------------
# Tooling. Under `sudo` the caller's PATH is usually reset, and node/pnpm on
# this box live in the user's home. Resolve them explicitly rather than failing
# with "tauri: command not found" at the least convenient moment.
# ---------------------------------------------------------------------------
NODE_BIN="${BUZZ_NODE:-$(command -v node || true)}"
[[ -x "${NODE_BIN}" ]] || NODE_BIN=/home/hermes/.local/bin/node
PNPM_BIN="${BUZZ_PNPM:-$(command -v pnpm || true)}"
[[ -x "${PNPM_BIN}" ]] || PNPM_BIN=/home/hermes/.hermes/node/bin/pnpm
[[ -x "${NODE_BIN}" ]] || die "node not found (set BUZZ_NODE)"
[[ -x "${PNPM_BIN}" ]] || die "pnpm not found (set BUZZ_PNPM)"
PATH="$(dirname "${NODE_BIN}"):$(dirname "${PNPM_BIN}"):${PATH}"
export PATH
[[ -d "${DESKTOP_DIR}/node_modules/@tauri-apps/cli" ]] || die "desktop deps not installed — run 'pnpm install --frozen-lockfile' in ${REPO_ROOT} first"

args=(signer generate -w "${KEYFILE}")
if $NO_PASSWORD; then
  say "WARNING: generating an UNPROTECTED key. Anyone who reads the file can sign updates that this box's users will install without a prompt."
  args+=(--password "")
elif $PASSWORD_SET; then
  args+=(--password "${PASSWORD}")
fi
# No --ci: without an explicit password we WANT the interactive prompt. `--ci`
# would silently produce an unprotected key, which is exactly the decision that
# should never be made by a default.

say "generating ${KEYFILE}"
set +e
# umask, not chmod-after: closes the window where the key exists 0644 on disk.
output="$( umask 077; cd "${DESKTOP_DIR}" && "${PNPM_BIN}" tauri "${args[@]}" 2>&1 )"
status=$?
set -e

if [[ ${status} -ne 0 ]]; then
  printf '%s\n' "${output}" >&2
  die "tauri signer generate failed (exit ${status})"
fi

[[ -s "${KEYFILE}" ]] || die "generator reported success but ${KEYFILE} is missing or empty"
[[ -s "${PUBFILE}" ]] || die "generator reported success but ${PUBFILE} is missing or empty"

# ---------------------------------------------------------------------------
# Rule 3: prove the captured output does not contain the private key before
# relaying a single byte of it.
# ---------------------------------------------------------------------------
leaked=false
while IFS= read -r line; do
  [[ -z "${line}" ]] && continue
  if printf '%s' "${output}" | grep -Fq -- "${line}"; then leaked=true; break; fi
done <"${KEYFILE}"

if $leaked; then
  say "REDACTED: the generator echoed private key material; its output is being withheld."
else
  printf '%s\n' "${output}" | sed 's/^/  | /'
fi

chmod 0600 "${KEYFILE}"
chmod 0644 "${PUBFILE}"

if [[ "$(id -u)" -eq 0 ]]; then
  chown root:root "${KEYFILE}" "${PUBFILE}"
  dir_owner="$(stat -c '%U:%G' "${KEYDIR}")"
  dir_mode="$(stat -c '%a' "${KEYDIR}")"
  if [[ "${dir_owner}" != "root:root" || "${dir_mode}" != "700" ]]; then
    say "WARNING: ${KEYDIR} is ${dir_owner} mode ${dir_mode}. A non-root user with write access to this directory can DELETE and REPLACE the key even though it is 0600. Harden with:"
    say "    sudo chown root:root ${KEYDIR} && sudo chmod 0700 ${KEYDIR}"
  fi
else
  say "NOTE: not running as root — the key is $(stat -c '%U:%G' "${KEYFILE}")-owned. Complete the handover with:"
  say "    sudo chown root:root ${KEYFILE} ${PUBFILE} ${KEYDIR}"
  say "    sudo chmod 0700 ${KEYDIR} && sudo chmod 0600 ${KEYFILE}"
fi

PUBKEY="$(tr -d '\r\n' <"${PUBFILE}")"

cat <<EOF

  Private key : ${KEYFILE}   (0600 — never leaves this box, never enters git)
  Public key  : ${PUBFILE}
  Public value: ${PUBKEY}

  NEXT:
    1. Back the private key up off this box, encrypted. If it is lost, every
       installed Buzz stops being updatable and the only fix is a manual
       reinstall on every machine.
    2. Build an updater-enabled bundle:
         eval "\$(${SCRIPT_DIR}/render-updater-config.sh --print-env)"
         export TAURI_SIGNING_PRIVATE_KEY="\$(sudo cat ${KEYFILE})"
         export TAURI_SIGNING_PRIVATE_KEY_PASSWORD='…'
       then the build command in ${SCRIPT_DIR}/README.md.
    3. Publish with ${SCRIPT_DIR}/publish-update.sh

EOF
