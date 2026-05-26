#!/bin/bash
# 401-dhcpv4-vs-static-lifetime.sh
# Integration test: DHCP-acquired addresses have finite kernel valid_lft while
# static policy addresses retain valid_lft forever.
# Mapped to acceptance criteria: "DHCP lifetime fields are handled correctly
# alongside static policies".
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/401-dhcpv4-vs-static-lifetime.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 401-dhcpv4-vs-static-lifetime: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 401-dhcpv4-vs-static-lifetime: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 401-dhcpv4-vs-static-lifetime: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; cleanup; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$POLICY_DIR" "$JOURNAL_DIR"

# DHCP interface pair: veth-dhcp0 (client) / veth-dhcp1 (server).
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.0.1/24

# Static interface pair: veth-static0 (managed) / veth-static1 (unmanaged end).
create_veth veth-static0 veth-static1

# Start dnsmasq DHCP server on the server-side interface.
start_dnsmasq veth-dhcp1 10.99.0.1 10.99.0.100 10.99.0.200 120

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 401-dhcpv4-vs-static-lifetime: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 401-dhcpv4-vs-static-lifetime: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Apply both policies from a directory.
APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: lifetime-dhcp
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

cat > "$APPLY_DIR/static.yaml" <<'EOF'
kind: policy
name: lifetime-static
factory: static
priority: 100
state:
  type: ethernet
  name: veth-static0
  addresses:
    - "10.99.1.1/24"
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$APPLY_DIR/"

# Wait for the DHCP lease to appear.
wait_for_address veth-dhcp0 "10.99.0." 10

# Wait for the static address to appear.
wait_for_address veth-static0 "10.99.1.1" 5

# Allow a moment for addresses to be fully applied.
sleep 1

# DHCP address must have a finite valid_lft (not "forever").
assert_valid_lft_finite veth-dhcp0 "10.99.0."

# Static address must have valid_lft forever.
assert_valid_lft_forever veth-static0 "10.99.1.1"

echo "PASS: 401-dhcpv4-vs-static-lifetime"
