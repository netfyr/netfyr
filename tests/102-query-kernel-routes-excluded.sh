#!/bin/bash
# 102-query-kernel-routes-excluded.sh
# Integration test: Verify that kernel-managed routes (proto kernel) are excluded
# from query output, that non-kernel routes appear, and that the "protocol" field
# is stripped from every returned route.
#
# Mapped to spec acceptance criteria:
#   "And kernel-managed routes (protocol=kernel) are excluded"
#   "And the protocol field is stripped from all returned routes"
#
# Setup:
#   - veth-test0 gets address 10.88.0.1/24; kernel auto-installs 10.88.0.0/24
#     (proto kernel, scope link) — this route must NOT appear in query output.
#   - An explicit static route 10.77.0.0/24 dev veth-test0 is added via `ip route`;
#     this route MUST appear in query output (proto boot/static, not kernel).
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/102-query-kernel-routes-excluded.sh
#   bash tests/102-query-kernel-routes-excluded.sh   (uses target/debug/netfyr fallback)

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

# Create a veth pair and add an address.
# Adding 10.88.0.1/24 causes the kernel to auto-install 10.88.0.0/24 (proto kernel).
create_veth veth-test0 veth-test1
add_address veth-test0 10.88.0.1/24

# Add an explicit link-scoped route that is not kernel-managed.
# `ip route add` without `proto` uses proto boot, which is not "kernel".
ip route add 10.77.0.0/24 dev veth-test0 scope link

# Query the specific interface in JSON mode.
output=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

# Assert: the "ipv4" sub-object is present.
if ! echo "$output" | grep -q '"ipv4"'; then
    echo "FAIL: 102-query-kernel-routes-excluded: output does not contain 'ipv4' sub-object" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: the explicitly added static route (10.77.0.0/24) IS present in the output.
# The destination "10.77.0.0/24" should only appear in the routes section.
if ! echo "$output" | grep -q '10\.77\.0\.0/24'; then
    echo "FAIL: 102-query-kernel-routes-excluded: static route 10.77.0.0/24 is missing from output" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: the kernel-managed connected route (10.88.0.0/24) is NOT present in routes.
# The address 10.88.0.1/24 will appear in "ipv4.addresses", but the network 10.88.0.0/24
# (the kernel-installed connected route) should be absent from "ipv4.routes".
if echo "$output" | grep -q '10\.88\.0\.0/24'; then
    echo "FAIL: 102-query-kernel-routes-excluded: kernel route 10.88.0.0/24 appears in output but should be excluded" >&2
    echo "Output: $output" >&2
    exit 1
fi

# Assert: the "protocol" key is NOT present in the output.
# All routes that pass the kernel filter have their protocol field stripped,
# so "protocol" must not appear anywhere in the JSON output.
if echo "$output" | grep -q '"protocol"'; then
    echo "FAIL: 102-query-kernel-routes-excluded: 'protocol' field appears in output but must be stripped" >&2
    echo "Output: $output" >&2
    exit 1
fi

echo "PASS: 102-query-kernel-routes-excluded"
