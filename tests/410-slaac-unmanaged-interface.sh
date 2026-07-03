#!/bin/bash
# 410-slaac-unmanaged-interface.sh
# Integration test: An IPv6 auto policy for one interface does not disturb
# another unmanaged interface's configuration.
# Mapped to acceptance criteria: "SLAAC does not tear down unmanaged interfaces".
#
# Requires: unshare, ip (iproute2), radvd
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/410-slaac-unmanaged-interface.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

if ! command -v radvd >/dev/null 2>&1; then
    echo "FAIL: radvd not found; install radvd to run SLAAC integration tests" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace.
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'kill "${DAEMON_PID:-}" "${RADVD_PID:-}" 2>/dev/null; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create SLAAC veth pair (managed by ipv6auto policy).
create_veth veth-slaac0 veth-slaac1
ip link set dev veth-slaac1 up

# Create unmanaged veth pair with a custom MTU.
create_veth veth-other0 veth-other1
ip link set dev veth-other0 mtu 1400

# Configure radvd on veth-slaac1.
RADVD_CONF="$TMPDIR_TEST/radvd.conf"
RADVD_PID_FILE="$TMPDIR_TEST/radvd.pid"

cat > "$RADVD_CONF" <<EOF
interface veth-slaac1 {
    AdvSendAdvert on;
    MinRtrAdvInterval 3;
    MaxRtrAdvInterval 10;
    prefix 2001:db8::/64 {
        AdvOnLink on;
        AdvAutonomous on;
        AdvValidLifetime 7200;
        AdvPreferredLifetime 3600;
    };
};
EOF

radvd -C "$RADVD_CONF" -p "$RADVD_PID_FILE" -n &
RADVD_PID=$!

# Write SLAAC policy for veth-slaac0 ONLY — no policy for veth-other0.
cat > "$POLICY_DIR/slaac.yaml" <<'EOF'
kind: policy
name: slaac-test
factory: ipv6auto
selector:
  name: veth-slaac0
EOF

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Wait for daemon socket.
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 410-slaac-unmanaged-interface: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Submit the SLAAC policy.
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/slaac.yaml"

# Wait up to 30 seconds for veth-slaac0 to get a SLAAC address (confirms daemon ran).
ADDR_WAIT=0
while ! ip -6 addr show dev veth-slaac0 2>/dev/null | grep -q "2001:db8::"; do
    if (( ADDR_WAIT >= 300 )); then
        echo "FAIL: 410-slaac-unmanaged-interface: veth-slaac0 did not acquire a SLAAC address within 30 seconds" >&2
        echo "      ip -6 addr show veth-slaac0:" >&2
        ip -6 addr show dev veth-slaac0 >&2 || true
        exit 1
    fi
    sleep 0.1
    (( ADDR_WAIT++ )) || true
done

# Assert veth-other0 is still UP and has MTU 1400 (daemon must not have touched it).
assert_link_up veth-other0

OTHER_MTU=$(ip link show dev veth-other0 | grep -oP 'mtu \K[0-9]+' || echo "unknown")
if [[ "$OTHER_MTU" != "1400" ]]; then
    echo "FAIL: 410-slaac-unmanaged-interface: veth-other0 mtu changed from 1400 to $OTHER_MTU" >&2
    echo "      ip link show veth-other0:" >&2
    ip link show dev veth-other0 >&2 || true
    exit 1
fi

echo "PASS: 410-slaac-unmanaged-interface"
