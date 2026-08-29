#!/usr/bin/env bash
set -euo pipefail

GATES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${GATES_DIR}/../../.." && pwd)"
GATE_TAG="test:harness-schema-contract"

# shellcheck source=lib/common.sh
source "${GATES_DIR}/lib/common.sh"
# shellcheck source=lib/harness.sh
source "${GATES_DIR}/lib/harness.sh"

EXPECTED_REL="scripts/reconcile-schema-after-pgschema.sql"
CANONICAL="${REPO_ROOT}/${EXPECTED_REL}"

[[ "${GATES_POST_SCHEMA_SCRIPT_REL}" == "${EXPECTED_REL}" ]] || {
  err "harness_schema must name ${EXPECTED_REL}, got ${GATES_POST_SCHEMA_SCRIPT_REL}"
  exit 1
}
[[ -f "${CANONICAL}" ]] || {
  err "canonical post-schema script is missing: ${EXPECTED_REL}"
  exit 1
}

preview_output="$(GATES_EXECUTE=0 harness_schema)"
grep -Fq -- "${EXPECTED_REL}" <<<"${preview_output}" || {
  err "harness_schema preview does not name the canonical post-schema script"
  exit 1
}
if grep -Fq -- "attach-schema-partitions.sql" <<<"${preview_output}"; then
  err "harness_schema preview still names the deleted partition-only script"
  exit 1
fi

grep -Fq -- "ATTACH PARTITION" "${CANONICAL}" || {
  err "canonical post-schema script no longer attaches partitions"
  exit 1
}
grep -Fq -- "ALTER TABLE replica_heartbeat SET (vacuum_truncate = false)" "${CANONICAL}" || {
  err "canonical post-schema script no longer converges heartbeat storage"
  exit 1
}
grep -Fq -- "INSERT INTO replica_heartbeat (id) VALUES (1)" "${CANONICAL}" || {
  err "canonical post-schema script no longer seeds the heartbeat singleton"
  exit 1
}
grep -Fq -- "RAISE EXCEPTION 'replica_heartbeat must disable vacuum truncation after pgschema apply'" "${CANONICAL}" || {
  err "canonical post-schema script no longer asserts heartbeat storage"
  exit 1
}
grep -Fq -- "RAISE EXCEPTION 'replica_heartbeat must contain its singleton row after pgschema apply'" "${CANONICAL}" || {
  err "canonical post-schema script no longer asserts the heartbeat singleton"
  exit 1
}

ok "harness schema contract uses existing canonical convergence with required invariants"
