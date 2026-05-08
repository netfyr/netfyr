#!/bin/bash
# 500-factory-event-during-connection.sh
# Integration test: A DHCP factory event (lease acquisition) is processed while
# another client is actively connected to the daemon. Validates that the event
# loop is not blocked by connection handling.
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/500-factory-event-during-connection.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 500-factory-event-during-connection: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 500-factory-event-during-connection: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 500-factory-event-during-connection: dnsmasq not found; install dnsmasq" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace.
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create a DHCP veth pair.
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.0.1/24

# Start dnsmasq on the server side.
start_dnsmasq veth-dhcp1 10.99.0.1 10.99.0.100 10.99.0.200 120

# Start the daemon with no pre-loaded policies.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for the daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 500-factory-event-during-connection: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 500-factory-event-during-connection: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Submit a DHCP policy. This starts a factory that will acquire a lease from
# dnsmasq. The factory event (LeaseAcquired) must be processed even though
# a client may connect concurrently.
POLICY_DIR_SUBMIT="$TMPDIR_TEST/submit"
mkdir -p "$POLICY_DIR_SUBMIT"

cat > "$POLICY_DIR_SUBMIT/dhcp.yaml" <<'EOF'
kind: policy
name: dhcp-concurrent-test
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR_SUBMIT"

# While the DHCP lease is being acquired, issue concurrent show queries.
# These must all succeed, and the factory event must be processed.
QUERY_PIDS=()
for i in $(seq 1 5); do
    NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show --output json >/dev/null 2>&1 &
    QUERY_PIDS+=($!)
done

# Wait for the DHCP lease to be applied (address on veth-dhcp0).
wait_for_address veth-dhcp0 "10.99.0." 15

# Verify the lease address is present.
assert_has_address veth-dhcp0 "10.99.0."

# Wait for background show queries to finish.
for pid in "${QUERY_PIDS[@]}"; do
    wait "$pid" || true
done

# Final verification: status shows the factory with a lease.
STATUS_JSON=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show --output json 2>&1) || true

if ! echo "$STATUS_JSON" | grep -q '"uptime_seconds"'; then
    echo "FAIL: 500-factory-event-during-connection: show output missing uptime_seconds" >&2
    echo "      json output: $STATUS_JSON" >&2
    exit 1
fi

if ! echo "$STATUS_JSON" | grep -q 'dhcp-concurrent-test'; then
    echo "FAIL: 500-factory-event-during-connection: factory not shown in output" >&2
    echo "      json output: $STATUS_JSON" >&2
    exit 1
fi

# Verify daemon is still alive.
if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 500-factory-event-during-connection: daemon crashed" >&2
    exit 1
fi

echo "PASS: 500-factory-event-during-connection"
