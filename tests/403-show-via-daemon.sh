#!/bin/bash
# 403-show-via-daemon.sh
# Integration test: `netfyr show` via the daemon returns a system overview
# including daemon status, uptime, all interfaces, and DHCP state for
# managed interfaces.
# Mapped to acceptance criteria:
#   "GetShowInfo returns system overview"
#   "Response includes daemon status 'running' with uptime"
#   "Interfaces includes all system interfaces"
#   "DHCP-managed interface has policies and dhcp fields"
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-show-via-daemon.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 403-show-via-daemon: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

# Create three veth pairs:
#   veth-stat0/veth-stat1  — managed by a static policy
#   veth-dhcp0/veth-dhcp1  — managed by a DHCPv4 policy
#   veth-free0/veth-free1  — unmanaged (no policy), appears in show output
create_veth veth-stat0 veth-stat1
create_veth veth-dhcp0 veth-dhcp1
create_veth veth-free0 veth-free1

# Configure the DHCP server side.
add_address veth-dhcp1 10.99.0.1/24
start_dnsmasq veth-dhcp1 10.99.0.1 10.99.0.100 10.99.0.200 120

start_daemon

# Submit two policies: one static, one DHCPv4.
POLICY_DIR_SUBMIT="$TMPDIR_TEST/submit"
mkdir -p "$POLICY_DIR_SUBMIT"

cat > "$POLICY_DIR_SUBMIT/static.yaml" <<'EOF'
kind: policy
name: show-static-mtu
factory: static
priority: 100
state:
  type: ethernet
  name: veth-stat0
  mtu: 1400
EOF

cat > "$POLICY_DIR_SUBMIT/dhcp.yaml" <<'EOF'
kind: policy
name: show-dhcp-lease
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_DIR_SUBMIT" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 403-show-via-daemon: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait for the DHCP lease (up to 10 seconds) so dhcp field is populated.
wait_for_address veth-dhcp0 "10.99.0." 10

# ── Phase 1: Text output ─────────────────────────────────────────────────────

SHOW_OUTPUT=$("$NETFYR_BIN" show 2>&1) || SHOW_EXIT=$?

if [[ -z "$SHOW_OUTPUT" ]]; then
    echo "FAIL: 403-show-via-daemon: netfyr show returned empty output" >&2
    exit 1
fi

# Daemon section must show "running" status and uptime.
if ! echo "$SHOW_OUTPUT" | grep -q "Status:"; then
    echo "FAIL: 403-show-via-daemon: text output missing 'Status:' line" >&2
    echo "      show output: $SHOW_OUTPUT" >&2
    exit 1
fi

if ! echo "$SHOW_OUTPUT" | grep -qi "running"; then
    echo "FAIL: 403-show-via-daemon: daemon is not shown as running in text output" >&2
    echo "      show output: $SHOW_OUTPUT" >&2
    exit 1
fi

if ! echo "$SHOW_OUTPUT" | grep -q "Uptime:"; then
    echo "FAIL: 403-show-via-daemon: text output missing 'Uptime:' line" >&2
    echo "      show output: $SHOW_OUTPUT" >&2
    exit 1
fi

# Interfaces section must include the managed interfaces.
if ! echo "$SHOW_OUTPUT" | grep -q "veth-stat0"; then
    echo "FAIL: 403-show-via-daemon: text output does not include veth-stat0" >&2
    echo "      show output: $SHOW_OUTPUT" >&2
    exit 1
fi

if ! echo "$SHOW_OUTPUT" | grep -q "veth-dhcp0"; then
    echo "FAIL: 403-show-via-daemon: text output does not include veth-dhcp0" >&2
    echo "      show output: $SHOW_OUTPUT" >&2
    exit 1
fi

# The DHCP-managed interface must show DHCP state.
if ! echo "$SHOW_OUTPUT" | grep -q "DHCP:"; then
    echo "FAIL: 403-show-via-daemon: text output missing 'DHCP:' line for DHCP-managed interface" >&2
    echo "      show output: $SHOW_OUTPUT" >&2
    exit 1
fi

# The static-managed interface must list its policy.
if ! echo "$SHOW_OUTPUT" | grep -q "Policies:"; then
    echo "FAIL: 403-show-via-daemon: text output missing 'Policies:' line" >&2
    echo "      show output: $SHOW_OUTPUT" >&2
    exit 1
fi

# ── Phase 2: JSON output ─────────────────────────────────────────────────────

SHOW_JSON=$("$NETFYR_BIN" show --output json 2>&1) || true

if [[ -z "$SHOW_JSON" ]]; then
    echo "FAIL: 403-show-via-daemon: netfyr show --output json returned empty output" >&2
    exit 1
fi

# JSON must include daemon.status = "running".
if ! echo "$SHOW_JSON" | grep -q '"running"'; then
    echo "FAIL: 403-show-via-daemon: JSON output does not contain 'running' status" >&2
    echo "      json output: $SHOW_JSON" >&2
    exit 1
fi

# JSON must include uptime_seconds.
if ! echo "$SHOW_JSON" | grep -q '"uptime_seconds"'; then
    echo "FAIL: 403-show-via-daemon: JSON output missing 'uptime_seconds' field" >&2
    echo "      json output: $SHOW_JSON" >&2
    exit 1
fi

# JSON must include the interfaces array.
if ! echo "$SHOW_JSON" | grep -q '"interfaces"'; then
    echo "FAIL: 403-show-via-daemon: JSON output missing 'interfaces' field" >&2
    echo "      json output: $SHOW_JSON" >&2
    exit 1
fi

# JSON must include both managed interface names.
if ! echo "$SHOW_JSON" | grep -q '"veth-stat0"'; then
    echo "FAIL: 403-show-via-daemon: JSON output missing veth-stat0 interface" >&2
    echo "      json output: $SHOW_JSON" >&2
    exit 1
fi

if ! echo "$SHOW_JSON" | grep -q '"veth-dhcp0"'; then
    echo "FAIL: 403-show-via-daemon: JSON output missing veth-dhcp0 interface" >&2
    echo "      json output: $SHOW_JSON" >&2
    exit 1
fi

echo "PASS: 403-show-via-daemon"
