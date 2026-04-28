#!/bin/bash
# helpers.sh -- Shared shell functions for netfyr integration tests.
# Source this file from test scripts: source "$(dirname "$0")/helpers.sh"
#
# Requires: bash, ip (iproute2), unshare (util-linux), grep
# Optional: dnsmasq (only for DHCP tests)

set -euo pipefail

# Array of dnsmasq PIDs started by start_dnsmasq, used by cleanup.
_DNSMASQ_PIDS=()

# netns_setup -- Run the calling script inside a new unprivileged user+network
# namespace. Uses exec re-entry guarded by __NETNS_ENTERED to avoid recursion.
# After re-entry, registers cleanup as a trap on EXIT.
netns_setup() {
    if [[ -n "${__NETNS_ENTERED:-}" ]]; then
        # Already inside the namespace; register cleanup and continue.
        trap cleanup EXIT
        return 0
    fi

    if ! command -v unshare >/dev/null 2>&1; then
        echo "FAIL: 'unshare' not found; install util-linux to run integration tests" >&2
        exit 1
    fi

    export __NETNS_ENTERED=1
    exec unshare --user --net --map-root-user -- "$0" "$@"
    # exec replaces the shell; code below is unreachable.
}

# create_veth VETH0 VETH1 -- Create a veth pair and bring both ends up.
create_veth() {
    local veth0="$1"
    local veth1="$2"
    ip link add "$veth0" type veth peer name "$veth1"
    ip link set "$veth0" up
    ip link set "$veth1" up
}

# add_address IFACE CIDR -- Add an IP address to a network interface.
add_address() {
    local iface="$1"
    local cidr="$2"
    ip addr add "$cidr" dev "$iface"
}

# start_dnsmasq IFACE SERVER_IP RANGE_START RANGE_END LEASE_TIME
# Start a DHCP server on IFACE. Exits 1 immediately if dnsmasq is not installed.
# Stores the PID in _DNSMASQ_PIDS for cleanup.
start_dnsmasq() {
    local iface="$1"
    local server_ip="$2"
    local range_start="$3"
    local range_end="$4"
    local lease_time="$5"

    if ! command -v dnsmasq >/dev/null 2>&1; then
        echo "FAIL: 'dnsmasq' not found; install dnsmasq to run DHCP integration tests" >&2
        exit 1
    fi

    local leasefile
    leasefile=$(mktemp)

    dnsmasq \
        --no-daemon \
        --bind-dynamic \
        --interface="$iface" \
        --dhcp-range="${range_start},${range_end},${lease_time}" \
        --dhcp-leasefile="$leasefile" \
        --no-resolv \
        --no-hosts \
        --log-dhcp \
        >/dev/null 2>&1 &

    local pid=$!
    _DNSMASQ_PIDS+=("$pid")

    # Brief pause to let dnsmasq bind to the interface before tests proceed.
    sleep 1
}

# cleanup -- Kill any running dnsmasq instances started by start_dnsmasq.
# Registered as a trap EXIT handler by netns_setup.
cleanup() {
    local pid
    for pid in "${_DNSMASQ_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    _DNSMASQ_PIDS=()
}

# assert_eq ACTUAL EXPECTED MSG -- Fail if ACTUAL != EXPECTED.
assert_eq() {
    local actual="$1"
    local expected="$2"
    local msg="$3"
    if [[ "$actual" != "$expected" ]]; then
        echo "FAIL: $msg: expected '$expected', got '$actual'" >&2
        exit 1
    fi
}

# assert_match VALUE PATTERN MSG -- Fail if VALUE does not match regex PATTERN.
assert_match() {
    local value="$1"
    local pattern="$2"
    local msg="$3"
    if [[ ! "$value" =~ $pattern ]]; then
        echo "FAIL: $msg: '$value' did not match pattern '$pattern'" >&2
        exit 1
    fi
}

# assert_has_address IFACE PREFIX -- Fail if IFACE does not have an address
# containing PREFIX (e.g. "10.99.0.").
assert_has_address() {
    local iface="$1"
    local prefix="$2"
    local output
    output=$(ip addr show dev "$iface" 2>&1) || true
    if ! echo "$output" | grep -qF "$prefix"; then
        echo "FAIL: interface '$iface' does not have an address matching '$prefix'" >&2
        echo "      ip addr output: $output" >&2
        exit 1
    fi
}

# assert_link_up IFACE -- Fail if IFACE is not in the UP state.
assert_link_up() {
    local iface="$1"
    local output
    output=$(ip link show dev "$iface" 2>&1) || true
    if ! echo "$output" | grep -qE "(state UP|,UP,|<[^>]*UP[^>]*>)"; then
        echo "FAIL: interface '$iface' is not UP" >&2
        echo "      ip link output: $output" >&2
        exit 1
    fi
}

# assert_not_has_address IFACE PREFIX -- Fail if IFACE has an address containing PREFIX.
assert_not_has_address() {
    local iface="$1"
    local prefix="$2"
    local output
    output=$(ip addr show dev "$iface" 2>&1) || true
    if echo "$output" | grep -qF "$prefix"; then
        echo "FAIL: interface '$iface' unexpectedly has an address matching '$prefix'" >&2
        echo "      ip addr output: $output" >&2
        exit 1
    fi
}

# assert_mtu IFACE EXPECTED_MTU -- Fail if IFACE does not have the expected MTU.
assert_mtu() {
    local iface="$1"
    local expected="$2"
    local output
    output=$(ip link show dev "$iface" 2>&1) || true
    if ! echo "$output" | grep -q "mtu $expected"; then
        echo "FAIL: interface '$iface' does not have mtu $expected" >&2
        echo "      ip link output: $output" >&2
        exit 1
    fi
}

# wait_for_address IFACE PREFIX TIMEOUT_SECONDS -- Poll until an address matching PREFIX
# appears on IFACE, or fail after TIMEOUT_SECONDS.
wait_for_address() {
    local iface="$1"
    local prefix="$2"
    local timeout_sec="$3"
    local max_iters=$(( timeout_sec * 10 ))
    local waited=0
    while ! ip addr show dev "$iface" 2>/dev/null | grep -qF "$prefix"; do
        if (( waited >= max_iters )); then
            echo "FAIL: interface '$iface' did not get an address matching '$prefix' within ${timeout_sec}s" >&2
            echo "      ip addr show $iface:" >&2
            ip addr show dev "$iface" >&2 || true
            exit 1
        fi
        sleep 0.1
        (( waited++ )) || true
    done
}

# wait_for_no_address IFACE PREFIX TIMEOUT_SECONDS -- Poll until an address matching PREFIX
# disappears from IFACE, or fail after TIMEOUT_SECONDS.
#
# Uses a subshell capture for the `ip addr show` output rather than a direct
# pipe to `grep -q`, to avoid a false-negative race under `set -o pipefail`
# that can occur when SIGCHLD from a recently-killed background process
# interrupts the pipeline's exit-code evaluation.
wait_for_no_address() {
    local iface="$1"
    local prefix="$2"
    local timeout_sec="$3"
    local max_iters=$(( timeout_sec * 10 ))
    local waited=0
    local _addr_out
    while true; do
        _addr_out=$(ip addr show dev "$iface" 2>/dev/null) || true
        if ! echo "$_addr_out" | grep -qF "$prefix"; then
            break
        fi
        if (( waited >= max_iters )); then
            echo "FAIL: interface '$iface' still has an address matching '$prefix' after ${timeout_sec}s" >&2
            echo "      ip addr show $iface:" >&2
            ip addr show dev "$iface" >&2 || true
            exit 1
        fi
        sleep 0.1
        (( waited++ )) || true
    done
}

# assert_address_count IFACE EXPECTED_COUNT -- Fail if the number of IPv4 (inet)
# addresses on IFACE does not match EXPECTED_COUNT. Uses "inet " (with trailing
# space) to exclude inet6 lines.
assert_address_count() {
    local iface="$1"
    local expected="$2"
    local output
    output=$(ip addr show dev "$iface" 2>&1) || true
    local count
    count=$(echo "$output" | grep -c "inet ") || count=0
    if [[ "$count" -ne "$expected" ]]; then
        echo "FAIL: interface '$iface' has $count inet address(es), expected $expected" >&2
        echo "      ip addr output: $output" >&2
        exit 1
    fi
}

# assert_json_address_order JSON_TEXT ADDR [ADDR ...] -- Fail if any ADDR is absent
# from JSON_TEXT or if the addresses do not appear in the given left-to-right order.
# Determines order via character-offset comparison using bash parameter expansion
# (no jq required). The pattern "${json%%addr*}" yields the text before the first
# occurrence of addr, so its length is the character offset of that occurrence.
assert_json_address_order() {
    local json_text="$1"
    shift
    local prev_offset=-1
    local addr before offset
    for addr in "$@"; do
        if ! echo "$json_text" | grep -qF "$addr"; then
            echo "FAIL: address '$addr' not found in JSON output" >&2
            echo "      JSON: $json_text" >&2
            exit 1
        fi
        before="${json_text%%"$addr"*}"
        offset="${#before}"
        if (( offset <= prev_offset )); then
            echo "FAIL: address '$addr' is not in expected position (offset $offset <= previous $prev_offset)" >&2
            echo "      JSON: $json_text" >&2
            exit 1
        fi
        prev_offset="$offset"
    done
}
