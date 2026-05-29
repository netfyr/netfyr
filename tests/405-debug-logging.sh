#!/bin/bash
# 405-debug-logging.sh -- Verify debug-level messages cover key daemon event flow.
#
# Requires: unshare, ip (iproute2), grep
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

netns_setup "$@"

# ---------- Inside the namespace ----------

require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1

DAEMON_STDERR="$TMPDIR_TEST/daemon.log"
start_daemon RUST_LOG=netfyr_daemon=debug

# Write and apply a static policy to trigger reconciliation ("diff computed").
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: debug-logging-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 405-debug-logging: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Trigger an external MTU change to exercise the netlink event flow:
# netlink event parsed → debounce timer fired → recording external changes.
ip link set veth-e2e0 mtu 1500

# Wait for the debounce window to expire (daemon uses ~500 ms debounce; 2 s is generous).
sleep 2

stop_daemon

# Assert all 5 required log patterns are present.
check_pattern() {
    local pattern="$1"
    if ! grep -q "$pattern" "$DAEMON_STDERR"; then
        echo "FAIL: 405-debug-logging: pattern \"$pattern\" not found in daemon log" >&2
        echo "      (daemon log contents follow)" >&2
        cat "$DAEMON_STDERR" >&2 || true
        exit 1
    fi
}

check_pattern "RTM_GETLINK dump"
check_pattern "netlink event parsed"
check_pattern "debounce timer fired"
check_pattern "recording external changes"
check_pattern "diff computed"

echo "PASS: 405-debug-logging"
