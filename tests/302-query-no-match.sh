#!/bin/bash
# 302-query-no-match.sh
# AC: "No matching entities returns empty result" — querying for an interface
# that does not exist must return an empty list and exit with code 0.
#
# Tests both JSON (explicit empty array "[]") and YAML (empty sequence) output,
# and verifies that the exit code is 0 (not an error).
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/302-query-no-match.sh
#   bash tests/302-query-no-match.sh   (uses target/debug/netfyr fallback)

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

# Force daemon-free mode — no daemon is running in this namespace.
export NETFYR_SOCKET_PATH=/nonexistent/netfyr.sock

# ── Test 1: JSON output for a non-existent name returns "[]" with exit code 0 ──

JSON_EXIT=0
JSON_OUTPUT=$("$NETFYR_BIN" query -s name=eth-does-not-exist-99 -o json) \
    || JSON_EXIT=$?

if [[ $JSON_EXIT -ne 0 ]]; then
    echo "FAIL: 302-query-no-match: expected exit code 0 for no-match JSON query, got $JSON_EXIT" >&2
    echo "      output: $JSON_OUTPUT" >&2
    exit 1
fi

if [[ "$JSON_OUTPUT" != "[]" ]]; then
    echo "FAIL: 302-query-no-match: expected empty JSON array '[]', got: $JSON_OUTPUT" >&2
    exit 1
fi

# ── Test 2: YAML output for a non-existent name returns empty sequence with exit code 0 ──

YAML_EXIT=0
YAML_OUTPUT=$("$NETFYR_BIN" query -s name=eth-does-not-exist-99) \
    || YAML_EXIT=$?

if [[ $YAML_EXIT -ne 0 ]]; then
    echo "FAIL: 302-query-no-match: expected exit code 0 for no-match YAML query, got $YAML_EXIT" >&2
    echo "      output: $YAML_OUTPUT" >&2
    exit 1
fi

# YAML serialization of an empty list produces "[]" or a bare sequence marker.
# It must not contain any interface names or field values.
if echo "$YAML_OUTPUT" | grep -q "eth-does-not-exist-99"; then
    echo "FAIL: 302-query-no-match: YAML output should not contain the queried name" >&2
    echo "      output: $YAML_OUTPUT" >&2
    exit 1
fi

echo "PASS: 302-query-no-match"
