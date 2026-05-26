#!/bin/bash
# 406-root-submit-allowed.sh
# Integration test: Root user (uid 0 inside the namespace) can submit policies
# via SubmitPolicies and have them applied normally.
# Mapped to acceptance criteria:
#   "Root user calls SubmitPolicies → request processed normally"
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/406-root-submit-allowed.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 406-root-submit-allowed: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 406-root-submit-allowed: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script as uid 0).
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup
create_veth veth-test0 veth-test1
start_daemon

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: test-root-submit
factory: static
priority: 100
state:
  type: ethernet
  name: veth-test0
  mtu: 1400
EOF

# Apply as root (uid 0 in this namespace) — must succeed.
APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?

if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 406-root-submit-allowed: root netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify the policy was applied to the kernel.
LINK_OUTPUT=$(ip link show veth-test0)
if ! echo "$LINK_OUTPUT" | grep -q "mtu 1400"; then
    echo "FAIL: 406-root-submit-allowed: veth-test0 does not have mtu 1400 after root apply" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

echo "PASS: 406-root-submit-allowed"
