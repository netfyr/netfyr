#!/bin/bash
# 403-status-via-daemon.sh
# Integration test: `netfyr status` returns daemon operational state
# including uptime, policy count, and (when present) factory details.
# Mapped to acceptance criteria:
#   "GetShowInfo returns system overview"
#   "Response includes daemon status 'running' with uptime"
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-status-via-daemon.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 403-status-via-daemon: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 403-status-via-daemon: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 403-status-via-daemon: dnsmasq not found; install dnsmasq to run this test" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create a DHCP veth pair: veth-dhcp0 (client) / veth-dhcp1 (server).
create_veth veth-dhcp0 veth-dhcp1
add_address veth-dhcp1 10.99.0.1/24

# Create a second interface for a static policy.
create_veth veth-stat0 veth-stat1

# Start dnsmasq on the server side.
start_dnsmasq veth-dhcp1 10.99.0.1 10.99.0.100 10.99.0.200 120

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for the daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 403-status-via-daemon: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 403-status-via-daemon: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Submit two policies: one static, one DHCPv4.
POLICY_DIR_SUBMIT="$TMPDIR_TEST/submit"
mkdir -p "$POLICY_DIR_SUBMIT"

cat > "$POLICY_DIR_SUBMIT/static.yaml" <<'EOF'
kind: policy
name: stat-mtu
factory: static
priority: 100
state:
  type: ethernet
  name: veth-stat0
  mtu: 1400
EOF

cat > "$POLICY_DIR_SUBMIT/dhcp.yaml" <<'EOF'
kind: policy
name: dhcp-veth
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" apply "$POLICY_DIR_SUBMIT"
APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 403-status-via-daemon: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Phase 1: Status with no acquired lease (factory in waiting state) ────────

STATUS_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" status 2>&1) || STATUS_EXIT=$?

if [[ -z "$STATUS_OUTPUT" ]]; then
    echo "FAIL: 403-status-via-daemon: netfyr status returned empty output" >&2
    exit 1
fi

# Response must include uptime and policy count.
if ! echo "$STATUS_OUTPUT" | grep -q "Uptime:"; then
    echo "FAIL: 403-status-via-daemon: status output does not include 'Uptime:' line" >&2
    echo "      status output: $STATUS_OUTPUT" >&2
    exit 1
fi

if ! echo "$STATUS_OUTPUT" | grep -q "Policies:"; then
    echo "FAIL: 403-status-via-daemon: status output does not include 'Policies:' line" >&2
    echo "      status output: $STATUS_OUTPUT" >&2
    exit 1
fi

# Must show 2 active policies (static + dhcpv4).
if ! echo "$STATUS_OUTPUT" | grep -qE "Policies:[[:space:]]+2"; then
    echo "FAIL: 403-status-via-daemon: expected 2 active policies in status output" >&2
    echo "      status output: $STATUS_OUTPUT" >&2
    exit 1
fi

# The DHCPv4 factory must appear in the factory list.
if ! echo "$STATUS_OUTPUT" | grep -q "dhcp-veth"; then
    echo "FAIL: 403-status-via-daemon: DHCPv4 factory 'dhcp-veth' not shown in status" >&2
    echo "      status output: $STATUS_OUTPUT" >&2
    exit 1
fi

# The factory must be on the correct interface.
if ! echo "$STATUS_OUTPUT" | grep -q "veth-dhcp0"; then
    echo "FAIL: 403-status-via-daemon: factory interface 'veth-dhcp0' not shown in status" >&2
    echo "      status output: $STATUS_OUTPUT" >&2
    exit 1
fi

# ── Phase 2: Status JSON format ──────────────────────────────────────────────

STATUS_JSON=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" status --output json 2>&1) || true

if [[ -z "$STATUS_JSON" ]]; then
    echo "FAIL: 403-status-via-daemon: netfyr status --output json returned empty output" >&2
    exit 1
fi

# JSON must contain uptime_seconds as a non-negative value.
if ! echo "$STATUS_JSON" | grep -q '"uptime_seconds"'; then
    echo "FAIL: 403-status-via-daemon: JSON output missing 'uptime_seconds' field" >&2
    echo "      json output: $STATUS_JSON" >&2
    exit 1
fi

# JSON must contain active_policies = 2.
if ! echo "$STATUS_JSON" | grep -q '"active_policies": 2'; then
    echo "FAIL: 403-status-via-daemon: JSON output missing 'active_policies: 2'" >&2
    echo "      json output: $STATUS_JSON" >&2
    exit 1
fi

# JSON must contain running_factories as an array.
if ! echo "$STATUS_JSON" | grep -q '"running_factories"'; then
    echo "FAIL: 403-status-via-daemon: JSON output missing 'running_factories' field" >&2
    echo "      json output: $STATUS_JSON" >&2
    exit 1
fi

echo "PASS: 403-status-via-daemon"
