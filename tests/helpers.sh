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

# get_valid_lft IFACE CIDR_PREFIX -- Extract the valid_lft value for the first
# inet address matching CIDR_PREFIX on IFACE.  Returns the raw value
# (e.g. "120sec" or "forever").  Exits 1 if no matching address found.
get_valid_lft() {
    local iface="$1"
    local prefix="$2"
    local output
    output=$(ip addr show dev "$iface" 2>&1)
    local found=0
    while IFS= read -r line; do
        if [[ $found -eq 1 ]]; then
            local vlft
            vlft=$(echo "$line" | grep -oP 'valid_lft \K\S+') || true
            if [[ -n "$vlft" ]]; then
                echo "$vlft"
                return 0
            fi
            found=0
        fi
        if echo "$line" | grep -q "inet " && echo "$line" | grep -qF "$prefix"; then
            found=1
        fi
    done <<< "$output"
    echo "FAIL: get_valid_lft: no address matching '$prefix' on '$iface'" >&2
    return 1
}

# get_valid_lft_secs IFACE CIDR_PREFIX -- Like get_valid_lft but returns the
# numeric seconds (strips the "sec" suffix).  Returns 1 if the value is
# "forever" or no matching address is found.
get_valid_lft_secs() {
    local raw
    raw=$(get_valid_lft "$1" "$2") || return 1
    if [[ "$raw" == "forever" ]]; then
        echo "FAIL: get_valid_lft_secs: valid_lft is 'forever', expected numeric" >&2
        return 1
    fi
    echo "${raw%sec}"
}

# assert_valid_lft_finite IFACE CIDR_PREFIX -- Fail if the address's valid_lft
# is "forever" rather than a finite value.
assert_valid_lft_finite() {
    local iface="$1"
    local prefix="$2"
    local vlft
    vlft=$(get_valid_lft "$iface" "$prefix")
    if [[ "$vlft" == "forever" ]]; then
        echo "FAIL: address matching '$prefix' on '$iface' has valid_lft forever, expected finite" >&2
        ip addr show dev "$iface" >&2 || true
        exit 1
    fi
}

# assert_valid_lft_forever IFACE CIDR_PREFIX -- Fail if the address's valid_lft
# is not "forever".
assert_valid_lft_forever() {
    local iface="$1"
    local prefix="$2"
    local vlft
    vlft=$(get_valid_lft "$iface" "$prefix")
    if [[ "$vlft" != "forever" ]]; then
        echo "FAIL: address matching '$prefix' on '$iface' has valid_lft '$vlft', expected forever" >&2
        ip addr show dev "$iface" >&2 || true
        exit 1
    fi
}

# get_valid_lft6 IFACE CIDR_PREFIX -- Extract the valid_lft value for the first
# inet6 address matching CIDR_PREFIX on IFACE.  Returns the raw value
# (e.g. "120sec" or "forever").  Exits 1 if no matching address found.
get_valid_lft6() {
    local iface="$1"
    local prefix="$2"
    local output
    output=$(ip addr show dev "$iface" 2>&1)
    local found=0
    while IFS= read -r line; do
        if [[ $found -eq 1 ]]; then
            local vlft
            vlft=$(echo "$line" | grep -oP 'valid_lft \K\S+') || true
            if [[ -n "$vlft" ]]; then
                echo "$vlft"
                return 0
            fi
            found=0
        fi
        if echo "$line" | grep -q "inet6 " && echo "$line" | grep -qF "$prefix"; then
            found=1
        fi
    done <<< "$output"
    echo "FAIL: get_valid_lft6: no address matching '$prefix' on '$iface'" >&2
    return 1
}

# assert_valid_lft6_finite IFACE CIDR_PREFIX -- Fail if the inet6 address's
# valid_lft is "forever" rather than a finite value.
assert_valid_lft6_finite() {
    local iface="$1"
    local prefix="$2"
    local vlft
    vlft=$(get_valid_lft6 "$iface" "$prefix")
    if [[ "$vlft" == "forever" ]]; then
        echo "FAIL: inet6 address matching '$prefix' on '$iface' has valid_lft forever, expected finite" >&2
        ip addr show dev "$iface" >&2 || true
        exit 1
    fi
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

# require_binaries -- Resolve NETFYR_BIN and NETFYR_DAEMON_BIN from environment
# or default paths, then validate both exist and are executable. Exits 1 if
# either is missing. SCRIPT_DIR must be set before calling (standard convention).
require_binaries() {
    NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
    NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

    if [[ ! -x "$NETFYR_BIN" ]]; then
        echo "FAIL: netfyr binary not found at $NETFYR_BIN" >&2
        exit 1
    fi
    if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
        echo "FAIL: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
        exit 1
    fi
}

# daemon_test_setup -- Create temp directories, export socket/policy env vars,
# and register an EXIT trap that cleans up the daemon and temp files.
# Sets: TMPDIR_TEST, SOCKET_PATH, POLICY_DIR
# Exports: NETFYR_SOCKET_PATH, NETFYR_POLICY_DIR
daemon_test_setup() {
    TMPDIR_TEST=$(mktemp -d)
    SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
    POLICY_DIR="$TMPDIR_TEST/policies"
    mkdir -p "$POLICY_DIR"

    export NETFYR_SOCKET_PATH="$SOCKET_PATH"
    export NETFYR_POLICY_DIR="$POLICY_DIR"

    DAEMON_PID=""

    trap '_daemon_test_cleanup' EXIT
}

# _daemon_test_cleanup -- Private EXIT trap handler installed by daemon_test_setup.
# Kills the daemon, calls cleanup (kills dnsmasq), and removes the temp directory.
_daemon_test_cleanup() {
    if [[ -n "${DAEMON_PID:-}" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    cleanup
    rm -rf "${TMPDIR_TEST:-}"
}

# setup_journal -- Create a journal subdirectory and export NETFYR_JOURNAL_DIR.
# Must be called after daemon_test_setup.
# Sets: JOURNAL_DIR
# Exports: NETFYR_JOURNAL_DIR
setup_journal() {
    JOURNAL_DIR="$TMPDIR_TEST/journal"
    mkdir -p "$JOURNAL_DIR"
    export NETFYR_JOURNAL_DIR="$JOURNAL_DIR"
}

# start_daemon [ENV_PAIRS...] -- Start the daemon in the background with the
# exported environment variables. Accepts optional extra KEY=VALUE env pairs.
# Waits up to 5 seconds for the socket to appear. Sets DAEMON_PID.
# Set DAEMON_STDERR=/path/to/file before calling to capture daemon stderr.
start_daemon() {
    local daemon_stderr="${DAEMON_STDERR:-/dev/null}"

    local env_args=(
        "NETFYR_SOCKET_PATH=$SOCKET_PATH"
        "NETFYR_POLICY_DIR=$POLICY_DIR"
    )
    if [[ -n "${NETFYR_JOURNAL_DIR:-}" ]]; then
        env_args+=("NETFYR_JOURNAL_DIR=$NETFYR_JOURNAL_DIR")
    fi

    env "${env_args[@]}" "$@" "$NETFYR_DAEMON_BIN" 2>"$daemon_stderr" &
    DAEMON_PID=$!

    local i
    for i in $(seq 1 50); do
        [[ -S "$SOCKET_PATH" ]] && return 0
        sleep 0.1
    done
    echo "FAIL: daemon socket did not appear at $SOCKET_PATH" >&2
    exit 1
}

# stop_daemon -- Kill the daemon and wait for it to exit. Clears DAEMON_PID.
# Removes the socket file so a subsequent start_daemon creates a fresh one.
stop_daemon() {
    if [[ -n "${DAEMON_PID:-}" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi
    rm -f "$SOCKET_PATH"
}

# restart_daemon [ENV_PAIRS...] -- Stop the current daemon and start a new one
# with the same directories. Passes extra env pairs to the new daemon instance.
restart_daemon() {
    stop_daemon
    start_daemon "$@"
}

# setup_dhcp_topology CLIENT_VETH SERVER_VETH SERVER_IP RANGE_START RANGE_END [LEASE_TIME]
# Convenience wrapper: creates a veth pair, assigns the server IP, and starts dnsmasq.
# LEASE_TIME defaults to 120 seconds.
setup_dhcp_topology() {
    local client_veth="$1" server_veth="$2"
    local server_ip="$3" range_start="$4" range_end="$5"
    local lease_time="${6:-120}"

    create_veth "$client_veth" "$server_veth"
    add_address "$server_veth" "${server_ip}/24"
    start_dnsmasq "$server_veth" "$server_ip" "$range_start" "$range_end" "$lease_time"
}
