#!/bin/bash
# 302-query-driver.sh
# AC: "Query with driver selector" — using --selector driver=<name> without a
# type= prefix exercises the all-entity-types iteration path in run_query_local.
#
# In an unprivileged namespace, veth interfaces have no PCI device and therefore
# no driver symlink in /sys/class/net/<name>/device/driver.  Querying with
# driver=ixgbe must therefore return an empty result (exit 0).
# A sanity check first confirms that the same interfaces do appear without the
# driver filter.
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/302-query-driver.sh
#   bash tests/302-query-driver.sh   (uses target/debug/netfyr fallback)

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

# Force daemon-free mode.
export NETFYR_SOCKET_PATH=/nonexistent/netfyr.sock

# Create a veth pair. Veth interfaces have no PCI driver symlink in sysfs.
create_veth veth-drv0 veth-drv1

# ── Sanity check: both veths appear without any selector ─────────────────────

ALL_OUTPUT=$("$NETFYR_BIN" query -o json)

if ! echo "$ALL_OUTPUT" | grep -q '"veth-drv0"'; then
    echo "FAIL: 302-query-driver: veth-drv0 not found in unfiltered query output" >&2
    echo "Output: $ALL_OUTPUT" >&2
    exit 1
fi

if ! echo "$ALL_OUTPUT" | grep -q '"veth-drv1"'; then
    echo "FAIL: 302-query-driver: veth-drv1 not found in unfiltered query output" >&2
    echo "Output: $ALL_OUTPUT" >&2
    exit 1
fi

# ── Test: driver=ixgbe (no type= selector) returns empty, exit 0 ─────────────

DRV_EXIT=0
DRV_OUTPUT=$("$NETFYR_BIN" query -s driver=ixgbe -o json) || DRV_EXIT=$?

if [[ $DRV_EXIT -ne 0 ]]; then
    echo "FAIL: 302-query-driver: expected exit code 0 for driver=ixgbe query, got $DRV_EXIT" >&2
    echo "      output: $DRV_OUTPUT" >&2
    exit 1
fi

# Veth interfaces have no ixgbe driver — output must be empty.
if echo "$DRV_OUTPUT" | grep -q '"veth-drv0"'; then
    echo "FAIL: 302-query-driver: veth-drv0 must not appear when driver=ixgbe required" >&2
    echo "Output: $DRV_OUTPUT" >&2
    exit 1
fi

if echo "$DRV_OUTPUT" | grep -q '"veth-drv1"'; then
    echo "FAIL: 302-query-driver: veth-drv1 must not appear when driver=ixgbe required" >&2
    echo "Output: $DRV_OUTPUT" >&2
    exit 1
fi

if [[ "$DRV_OUTPUT" != "[]" ]]; then
    echo "FAIL: 302-query-driver: expected empty JSON array '[]' for driver=ixgbe, got: $DRV_OUTPUT" >&2
    exit 1
fi

echo "PASS: 302-query-driver"
