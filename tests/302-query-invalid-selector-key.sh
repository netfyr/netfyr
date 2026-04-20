#!/bin/bash
# 302-query-invalid-selector-key.sh
# AC: "Invalid selector key shows error" — running
#   netfyr query --selector invalid_key=value
# must print an error listing valid selector keys and exit with code 2.
#
# No network namespace is needed; clap's value_parser rejects the argument
# before any async/netlink code runs.
#
# Usage:
#   NETFYR_BIN=./target/debug/netfyr bash tests/302-query-invalid-selector-key.sh
#   bash tests/302-query-invalid-selector-key.sh   (uses target/debug/netfyr fallback)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 302-query-invalid-selector-key: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode (no socket at this path).
export NETFYR_SOCKET_PATH=/nonexistent

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" query --selector invalid_key=value 2>&1) || EXIT_CODE=$?

# AC: exit code must be 2.
if [[ $EXIT_CODE -ne 2 ]]; then
    echo "FAIL: 302-query-invalid-selector-key: expected exit code 2, got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: error output must list the valid selector keys.
for key in type name driver mac pci_path; do
    if ! echo "$OUTPUT" | grep -q "$key"; then
        echo "FAIL: 302-query-invalid-selector-key: error message does not mention valid key '$key'" >&2
        echo "      output: $OUTPUT" >&2
        exit 1
    fi
done

echo "PASS: 302-query-invalid-selector-key"
