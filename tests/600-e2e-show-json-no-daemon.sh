#!/bin/bash
# 600-e2e-show-json-no-daemon.sh -- End-to-end: netfyr show -o json produces valid JSON when daemon is not running.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-show-json-no-daemon: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json-no-daemon: jq not found; install jq to run JSON validation tests" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

# Point socket path at a nonexistent path — daemon is not running.
SOCKET_PATH="$TMPDIR_TEST/nonexistent.sock"

# Create a veth pair so there are real interfaces to list.
create_veth veth-e2e0 veth-e2e1

# Run netfyr show -o json (no daemon) — expect exit 0.
SHOW_EXIT=0
JSON_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" "$NETFYR_BIN" show -o json 2>&1) || SHOW_EXIT=$?

if [[ $SHOW_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-show-json-no-daemon: netfyr show -o json exited with code $SHOW_EXIT (expected 0)" >&2
    echo "      output: $JSON_OUTPUT" >&2
    exit 1
fi

# Validate it is a JSON object.
if ! echo "$JSON_OUTPUT" | jq -e 'type == "object"' >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-show-json-no-daemon: output is not a valid JSON object" >&2
    echo "      output: $JSON_OUTPUT" >&2
    exit 1
fi

# Verify daemon.status = "not_running".
DAEMON_STATUS=$(echo "$JSON_OUTPUT" | jq -r '.daemon.status')
if [[ "$DAEMON_STATUS" != "not_running" ]]; then
    echo "FAIL: 600-e2e-show-json-no-daemon: daemon.status is '$DAEMON_STATUS', expected 'not_running'" >&2
    exit 1
fi

# Verify daemon object does NOT have uptime_seconds.
HAS_UPTIME=$(echo "$JSON_OUTPUT" | jq '.daemon | has("uptime_seconds")')
if [[ "$HAS_UPTIME" != "false" ]]; then
    echo "FAIL: 600-e2e-show-json-no-daemon: daemon object must not have 'uptime_seconds' when not running" >&2
    echo "      daemon: $(echo "$JSON_OUTPUT" | jq '.daemon')" >&2
    exit 1
fi

# Verify interfaces is a non-empty array.
IFACE_COUNT=$(echo "$JSON_OUTPUT" | jq '.interfaces | length')
if [[ "$IFACE_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-show-json-no-daemon: interfaces array is empty" >&2
    exit 1
fi

# Verify each interface has only a "name" field (no "policies" or "dhcp").
HAS_POLICIES=$(echo "$JSON_OUTPUT" | jq '[.interfaces[] | has("policies")] | any')
if [[ "$HAS_POLICIES" != "false" ]]; then
    echo "FAIL: 600-e2e-show-json-no-daemon: some interface has 'policies' field (expected bare name only)" >&2
    echo "      interfaces: $(echo "$JSON_OUTPUT" | jq '.interfaces')" >&2
    exit 1
fi

HAS_DHCP=$(echo "$JSON_OUTPUT" | jq '[.interfaces[] | has("dhcp")] | any')
if [[ "$HAS_DHCP" != "false" ]]; then
    echo "FAIL: 600-e2e-show-json-no-daemon: some interface has 'dhcp' field (expected bare name only)" >&2
    echo "      interfaces: $(echo "$JSON_OUTPUT" | jq '.interfaces')" >&2
    exit 1
fi

echo "PASS: 600-e2e-show-json-no-daemon"
