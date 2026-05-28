#!/bin/bash
# 301-apply-static-policy.sh
# AC: "Apply static policy in namespace" (daemon-free mode)
#
# Runs netfyr apply without a daemon. Verifies that a static mtu policy
# is applied directly via netlink and that the exit code is 0.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-apply-static-policy: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode: socket path points to a location that does not exist.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

POLICY_FILE=$(mktemp --suffix=.yaml)
trap 'rm -f "$POLICY_FILE"' EXIT

create_veth veth-test0 veth-test1

cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: test-mtu
factory: static
priority: 100
state:
  type: ethernet
  name: veth-test0
  mtu: 1400
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?

if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 301-apply-static-policy: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-test0 1400

echo "PASS: 301-apply-static-policy"
