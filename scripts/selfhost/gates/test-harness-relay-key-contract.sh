#!/usr/bin/env bash
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="test:harness-relay-key-contract"
TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/buzz-gates-relay-key.XXXXXX")"
trap 'rm -rf "${TEST_DIR}"' EXIT

# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"
# shellcheck source=lib/harness.sh
source "${GATES_DIR}/lib/harness.sh"

mkdir -p "${TEST_DIR}/bin"
export FAKE_CARGO_COUNT="${TEST_DIR}/cargo-count"
cat > "${TEST_DIR}/bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == "run --quiet -p buzz-admin -- generate-key" ]] || exit 64
count=0
[[ ! -f "${FAKE_CARGO_COUNT}" ]] || read -r count < "${FAKE_CARGO_COUNT}"
printf '%s\n' "$((count + 1))" > "${FAKE_CARGO_COUNT}"
printf 'Public key:  '
printf 'b%.0s' {1..64}
printf '\nSecret key:  '
printf 'a%.0s' {1..64}
printf '\n\nSet BUZZ_PRIVATE_KEY to the secret key to use this identity.\n'
FAKE_CARGO
chmod 0755 "${TEST_DIR}/bin/cargo"

cat > "${TEST_DIR}/fake-relay" <<'FAKE_RELAY'
#!/usr/bin/env bash
set -euo pipefail
[[ "${BUZZ_RELAY_PRIVATE_KEY:-}" =~ ^[0-9a-f]{64}$ ]]
printf 'relay-key-shape=valid\n'
FAKE_RELAY
chmod 0755 "${TEST_DIR}/fake-relay"

PATH="${TEST_DIR}/bin:${PATH}"
export PATH

harness_relay_key_ensure
first_key="${GATES_RELAY_PRIVATE_KEY}"
[[ "${first_key}" =~ ^[0-9a-f]{64}$ ]] || {
  err "relay key minter did not retain a valid 32-byte hex key"
  exit 1
}
harness_relay_key_ensure
[[ "${GATES_RELAY_PRIVATE_KEY}" == "${first_key}" ]] || {
  err "relay key changed within one gate process"
  exit 1
}
[[ "$(<"${FAKE_CARGO_COUNT}")" == "1" ]] || {
  err "buzz-admin key semantics were invoked more than once"
  exit 1
}

relay_output="$(harness_relay_exec "${TEST_DIR}/fake-relay")"
[[ "${relay_output}" == "relay-key-shape=valid" ]] || {
  err "valid key shape did not reach the candidate relay environment"
  exit 1
}
[[ "${relay_output}" != *"${first_key}"* ]] || {
  err "relay key leaked to candidate relay output"
  exit 1
}
if grep -RFq -f /dev/stdin "${TEST_DIR}" <<<"${first_key}"; then
  err "relay key was persisted by the harness boundary"
  exit 1
fi

harness_functions="$(declare -f harness_relay_key_ensure harness_relay_exec harness_relay_start)"
for forbidden in /opt/buzz/keys /opt/buzz/relay /etc BUZZ_RELAY_PRIVATE_KEY:-; do
  [[ "${harness_functions}" != *"${forbidden}"* ]] || {
    err "relay key harness references forbidden production input: ${forbidden}"
    exit 1
  }
done

for caller in gate-conformance.sh gate-skew.sh gate-soak.sh; do
  grep -Fq -- "harness_relay_start" "${GATES_DIR}/${caller}" || {
    err "${caller} bypasses the shared candidate relay start path"
    exit 1
  }
  if grep -Eq -- '^[[:space:]]*(export[[:space:]]+)?BUZZ_RELAY_PRIVATE_KEY=|^[[:space:]]*(source|read|cat)[[:space:]].*/opt/buzz/keys' "${GATES_DIR}/${caller}"; then
    err "${caller} handles relay signing keys outside the shared harness"
    exit 1
  fi
done

ok "candidate relay key is source-minted, in-memory, source-shared and withheld"
