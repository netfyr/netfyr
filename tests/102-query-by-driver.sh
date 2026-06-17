#!/bin/bash
# 102-query-by-driver.sh
# Integration test: Verify that the driver= selector filters query results.
# Mapped to spec acceptance criterion:
#   "Scenario: Query by driver selector
#    Given ethernet interface 'eth0' using driver 'ixgbe' and 'eth1' using driver 'e1000'
#    When query is called with selector driver='ixgbe'
#    Then the result contains exactly one entity with name 'eth0'"
#
# In an unprivileged namespace, veth interfaces have no PCI device and therefore
# no driver symlink in /sys/class/net/<name>/device/driver. Querying with
# driver=ixgbe must return no results. This proves the driver selector is applied.
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-by-driver.sh
#   bash tests/102-query-by-driver.sh   (uses target/debug/netfyr fallback)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace (re-executes this script inside).
netns_setup "$@"

# ---------- Inside the namespace ----------

# Create a veth pair. Veth interfaces have no PCI driver symlink in sysfs,
# so driver= selectors will not match them.
create_veth veth-drv0 veth-drv1

# Sanity check: confirm both veth interfaces appear without any driver filter.
all_output=$("$NETFYR_BIN" query --selector type=ethernet --output json)

if ! echo "$all_output" | grep -q '"veth-drv0"'; then
    echo "FAIL: 102-query-by-driver: veth-drv0 not found without driver filter" >&2
    echo "Output: $all_output" >&2
    exit 1
fi

if ! echo "$all_output" | grep -q '"veth-drv1"'; then
    echo "FAIL: 102-query-by-driver: veth-drv1 not found without driver filter" >&2
    echo "Output: $all_output" >&2
    exit 1
fi

# Query with driver=ixgbe — no veth interface has this driver, so the result
# must be an empty JSON array.
driver_output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector driver=ixgbe \
    --output json)

if echo "$driver_output" | grep -q '"veth-drv0"'; then
    echo "FAIL: 102-query-by-driver: veth-drv0 must not appear when driver=ixgbe required (veth has no driver)" >&2
    echo "Output: $driver_output" >&2
    exit 1
fi

if echo "$driver_output" | grep -q '"veth-drv1"'; then
    echo "FAIL: 102-query-by-driver: veth-drv1 must not appear when driver=ixgbe required (veth has no driver)" >&2
    echo "Output: $driver_output" >&2
    exit 1
fi

# Assert: driver selector with no matching interface produces an empty JSON array.
if [[ "$driver_output" != "[]" ]]; then
    echo "FAIL: 102-query-by-driver: expected empty JSON array '[]' for driver=ixgbe, got: $driver_output" >&2
    exit 1
fi

echo "PASS: 102-query-by-driver"
