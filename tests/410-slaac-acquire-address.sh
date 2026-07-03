#!/bin/bash
# 410-slaac-acquire-address.sh
# Integration test: IPv6 auto factory acquires a SLAAC address from a Router
# Advertisement in an unprivileged user+network namespace.
# Mapped to acceptance criteria: "Factory acquires SLAAC address from Router
# Advertisement" and "SLAAC address DAD completes".
#
# Requires: unshare, ip (iproute2), radvd
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/410-slaac-acquire-address.sh

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

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'kill "${DAEMON_PID:-}" "${RADVD_PID:-}" 2>/dev/null; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create veth pair: veth-slaac0 (client/daemon) / veth-slaac1 (RA server).
create_veth veth-slaac0 veth-slaac1
ip link set dev veth-slaac1 up

# Configure radvd to advertise the 2001:db8::/64 prefix on veth-slaac1.
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
    RDNSS 2001:db8::53 {
        AdvRDNSSLifetime 3600;
    };
};
EOF

radvd -C "$RADVD_CONF" -p "$RADVD_PID_FILE" -n &
RADVD_PID=$!

# Write the IPv6 auto policy for veth-slaac0.
cat > "$POLICY_DIR/slaac.yaml" <<'EOF'
kind: policy
name: slaac-test
factory: ipv6auto
selector:
  name: veth-slaac0
EOF

# Start the daemon in background.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Wait for daemon socket to appear (poll up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 410-slaac-acquire-address: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Submit the SLAAC policy to the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/slaac.yaml"

# Wait up to 30 seconds for a SLAAC address in the 2001:db8:: prefix.
# SLAAC takes longer than DHCP: link-local DAD (~1 s) + RA receipt + SLAAC DAD (~1 s).
ADDR_WAIT=0
while ! ip -6 addr show dev veth-slaac0 2>/dev/null | grep -q "2001:db8::"; do
    if (( ADDR_WAIT >= 300 )); then
        echo "FAIL: 410-slaac-acquire-address: veth-slaac0 did not acquire a SLAAC address within 30 seconds" >&2
        echo "      ip -6 addr show veth-slaac0:" >&2
        ip -6 addr show dev veth-slaac0 >&2 || true
        exit 1
    fi
    sleep 0.1
    (( ADDR_WAIT++ )) || true
done

# Assert the address is present and not tentative.
if ! ip -6 addr show dev veth-slaac0 | grep "2001:db8::" | grep -qv "tentative"; then
    echo "FAIL: 410-slaac-acquire-address: SLAAC address is still tentative after 30 seconds" >&2
    ip -6 addr show dev veth-slaac0 >&2
    exit 1
fi

# Verify DNS servers from RDNSS are visible via netfyr query.
QUERY_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" query --selector name=veth-slaac0 2>&1)
if ! echo "$QUERY_OUTPUT" | grep -q "2001:db8::53"; then
    echo "FAIL: 410-slaac-acquire-address: netfyr query does not show RDNSS DNS server 2001:db8::53" >&2
    echo "      query output:" >&2
    echo "$QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 410-slaac-acquire-address"
