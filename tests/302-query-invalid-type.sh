#!/bin/bash
# 302-query-invalid-type.sh
# AC: "Invalid type value shows error" — running
#   netfyr query --selector type=foobar
# must print an error about the unknown entity type, list valid types, and
# exit with code 2.
#
# BackendRegistry returns UnsupportedEntityType immediately without making any
# netlink calls, so no network namespace is needed.
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/302-query-invalid-type.sh
#   bash tests/302-query-invalid-type.sh   (uses target/debug/netfyr fallback)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 302-query-invalid-type: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode (no socket at this path).
export NETFYR_SOCKET_PATH=/nonexistent

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" query --selector type=foobar 2>&1) || EXIT_CODE=$?

# AC: exit code must be 2.
if [[ $EXIT_CODE -ne 2 ]]; then
    echo "FAIL: 302-query-invalid-type: expected exit code 2, got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: error output must mention the unknown entity type "foobar".
if ! echo "$OUTPUT" | grep -qi "foobar"; then
    echo "FAIL: 302-query-invalid-type: error message does not mention the unknown type 'foobar'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: error output must list valid entity types (at minimum "ethernet").
if ! echo "$OUTPUT" | grep -q "ethernet"; then
    echo "FAIL: 302-query-invalid-type: error message does not list valid entity types" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

echo "PASS: 302-query-invalid-type"
