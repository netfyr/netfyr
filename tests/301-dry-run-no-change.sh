#!/bin/bash
# 301-dry-run-no-change.sh
# AC: "Dry-run does not change state in namespace" (daemon-free mode)
#
# Runs netfyr apply --dry-run without a daemon. Verifies that the mtu is
# NOT changed (still 1500) even though the policy requests 1400. Exit code
# must be 1 (changes are pending but not applied).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-dry-run-no-change: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

POLICY_FILE=$(mktemp --suffix=.yaml)
trap 'rm -f "$POLICY_FILE"' EXIT

create_veth veth-test0 veth-test1

cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: test-dryrun
factory: static
priority: 100
state:
  type: ethernet
  name: veth-test0
  mtu: 1400
EOF

# --dry-run with pending changes must exit 1.
DRY_RUN_EXIT=0
"$NETFYR_BIN" apply --dry-run "$POLICY_FILE" || DRY_RUN_EXIT=$?

if [[ $DRY_RUN_EXIT -ne 1 ]]; then
    echo "FAIL: 301-dry-run-no-change: expected exit code 1 from --dry-run with pending changes, got $DRY_RUN_EXIT" >&2
    exit 1
fi

# Interface MTU must be unchanged at the kernel default (1500).
assert_mtu veth-test0 1500

echo "PASS: 301-dry-run-no-change"
