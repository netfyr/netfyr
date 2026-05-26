#!/bin/bash
# 406-non-root-submit-denied.sh
# Integration test: Non-root user is denied when calling SubmitPolicies.
# Mapped to acceptance criteria:
#   "Non-root user calls SubmitPolicies → PermissionDenied error returned"
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/406-non-root-submit-denied.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 406-non-root-submit-denied: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 406-non-root-submit-denied: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

# Check that unprivileged user namespaces are supported before entering the
# outer namespace (netns_setup will also use unshare).
if ! unshare --user -- true 2>/dev/null; then
    echo "SKIP: 406-non-root-submit-denied: unprivileged user namespaces not available" >&2
    exit 0
fi

# Enter an unprivileged user+network namespace (re-executes this script as uid 0).
netns_setup "$@"

# ---------- Inside the namespace ----------

# Re-check nested user namespace support inside the outer namespace.
if ! unshare --user -- true 2>/dev/null; then
    echo "SKIP: 406-non-root-submit-denied: nested user namespaces not available" >&2
    exit 0
fi

daemon_test_setup
# chmod 755 so the nested non-root process (uid 65534) can traverse the
# directory to reach the socket.
chmod 755 "$TMPDIR_TEST"
create_veth veth-test0 veth-test1
start_daemon

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: test-non-root-submit
factory: static
priority: 100
state:
  type: ethernet
  name: veth-test0
  mtu: 1400
EOF
chmod 644 "$POLICY_FILE"

# Attempt to apply as a non-root user (uid 65534 in the outer namespace).
# Capture combined output; expect a non-zero exit code.
SUBMIT_ERR=""
SUBMIT_EXIT=0
SUBMIT_ERR=$(unshare --user -- "$NETFYR_BIN" apply "$POLICY_FILE" 2>&1) \
    || SUBMIT_EXIT=$?

if [[ $SUBMIT_EXIT -eq 0 ]]; then
    echo "FAIL: 406-non-root-submit-denied: non-root apply succeeded (expected failure)" >&2
    exit 1
fi

if ! echo "$SUBMIT_ERR" | grep -q "requires root"; then
    echo "FAIL: 406-non-root-submit-denied: expected 'requires root' in stderr, got:" >&2
    echo "      $SUBMIT_ERR" >&2
    exit 1
fi

# Verify the policy was NOT applied — MTU should remain at the default (1500).
LINK_OUTPUT=$(ip link show veth-test0)
if echo "$LINK_OUTPUT" | grep -q "mtu 1400"; then
    echo "FAIL: 406-non-root-submit-denied: veth-test0 has mtu 1400 after denied apply" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

echo "PASS: 406-non-root-submit-denied"
