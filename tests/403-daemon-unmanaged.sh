#!/bin/bash
# 403-daemon-unmanaged.sh
# Integration test: Interfaces not covered by any policy are completely
# untouched — MTU, address, and link state are preserved even when both
# static and DHCP policies are active on other interfaces.
# Mapped to spec scenario #27 and acceptance criteria:
#   "Interfaces without policies are not modified"
#   "End-to-end unmanaged interfaces"
#
# Requires: unshare, ip (iproute2), dnsmasq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-daemon-unmanaged.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

if ! command -v dnsmasq >/dev/null 2>&1; then
    echo "FAIL: 403-daemon-unmanaged: dnsmasq not found; install dnsmasq to run DHCP tests" >&2
    exit 1
fi

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

# Create three veth pairs:
#   veth-managed0/veth-managed1  -- managed by a static policy
#   veth-other0/veth-other1      -- unmanaged (no policy)
#   veth-dhcp0/veth-dhcp1        -- managed by a DHCPv4 policy
create_veth veth-managed0 veth-managed1
create_veth veth-other0 veth-other1
create_veth veth-dhcp0 veth-dhcp1

# Manually configure the unmanaged interface before the daemon starts.
ip link set dev veth-other0 mtu 1400
add_address veth-other0 10.99.2.1/24

# Set up DHCP server on veth-dhcp1.
add_address veth-dhcp1 10.99.0.1/24
start_dnsmasq veth-dhcp1 10.99.0.1 10.99.0.100 10.99.0.200 120

start_daemon

# Write a single YAML file with both policies (multi-document):
#   - static policy for veth-managed0 (mtu=1300)
#   - DHCPv4 policy for veth-dhcp0
POLICY_FILE="$TMPDIR_TEST/policies.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: managed-static
factory: static
priority: 100
state:
  type: ethernet
  name: veth-managed0
  mtu: 1300
---
kind: policy
name: dhcp-lease
factory: dhcpv4
selector:
  name: veth-dhcp0
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 403-daemon-unmanaged: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Wait up to 10 seconds for veth-dhcp0 to acquire a DHCP lease.
wait_for_address veth-dhcp0 "10.99.0." 10

# Assert managed interface received the static policy.
assert_mtu veth-managed0 1300

# Assert DHCP interface acquired a lease and is up.
assert_has_address veth-dhcp0 "10.99.0."
assert_link_up veth-dhcp0

# Assert unmanaged interface is completely unchanged.
assert_mtu veth-other0 1400
assert_has_address veth-other0 "10.99.2.1/24"
assert_link_up veth-other0

echo "PASS: 403-daemon-unmanaged"
