#!/bin/bash
# 600-e2e-history-dhcp-address-in-changes.sh
# End-to-end: DHCP lease acquisition shows the acquired address in the
# CHANGES column of "netfyr history", not only the default route.
#
# Reproduces a bug where addresses stored as JSON objects (with lifetime
# fields like preferred_lft/valid_lft) were silently dropped from the
# CHANGES column, while plain-string addresses displayed correctly.
#
# Requires: unshare, ip (iproute2), dnsmasq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-dhcp-address-in-changes: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-history-dhcp-address-in-changes: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-history-dhcp-address-in-changes: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; cleanup; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$POLICY_DIR" "$JOURNAL_DIR"

# Create veth pair: veth-dhcp0 is the client, veth-dhcp1 is the server.
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.1.1/24

# Start dnsmasq with a 120s lease.
start_dnsmasq veth-dhcp1 10.99.1.1 10.99.1.100 10.99.1.200 120

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
        echo "FAIL: 600-e2e-history-dhcp-address-in-changes: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-history-dhcp-address-in-changes: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Write a DHCP policy for veth-dhcp0.
cat > "$POLICY_DIR/dhcp.yaml" <<'EOF'
kind: policy
name: e2e-dhcp-hist
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

# Apply the policy.
APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR/dhcp.yaml" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-history-dhcp-address-in-changes: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for DHCP lease to be acquired (up to 10 seconds).
wait_for_address veth-dhcp0 "10.99.1." 10

# Give the daemon time to finish reconciliation and journal writes.
sleep 2

# ── Verify history output ──────────────────────────────────────────────────

HISTORY_OUTPUT=$(NO_COLOR=1 \
    NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 10 2>&1)

# Find the dhcp-acquire line (the DHCP lease acquisition event).
DHCP_LINE=$(echo "$HISTORY_OUTPUT" | grep "dhcp-acquire" | head -n 1)

if [[ -z "$DHCP_LINE" ]]; then
    echo "FAIL: 600-e2e-history-dhcp-address-in-changes: no dhcp-acquire entry found in history output" >&2
    echo "      output:" >&2
    echo "$HISTORY_OUTPUT" >&2
    exit 1
fi

# The CHANGES column must show the default route.
if ! echo "$DHCP_LINE" | grep -qF "+dflt via 10.99.1.1"; then
    echo "FAIL: 600-e2e-history-dhcp-address-in-changes: CHANGES column does not show '+dflt via 10.99.1.1'" >&2
    echo "      dhcp-lease line: $DHCP_LINE" >&2
    echo "      full output:" >&2
    echo "$HISTORY_OUTPUT" >&2
    exit 1
fi

# The CHANGES column must also show the acquired address.
if ! echo "$DHCP_LINE" | grep -qP '\+10\.99\.1\.\d+/24'; then
    echo "FAIL: 600-e2e-history-dhcp-address-in-changes: CHANGES column does not show the acquired address (+10.99.1.x/24)" >&2
    echo "      dhcp-lease line: $DHCP_LINE" >&2
    echo "      full output:" >&2
    echo "$HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-history-dhcp-address-in-changes"
