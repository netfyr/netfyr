#!/bin/bash
# 301-no-subcommand.sh
# AC: "No subcommand shows usage help, exit code 2"
#
# Running bare "netfyr" with no subcommand must print usage help and exit 2.
# Clap's SubcommandRequiredElseHelp policy handles this automatically.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-no-subcommand: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" 2>&1) || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 2 ]]; then
    echo "FAIL: 301-no-subcommand: expected exit code 2, got $EXIT_CODE" >&2
    exit 1
fi

if ! echo "$OUTPUT" | grep -qi "usage"; then
    echo "FAIL: 301-no-subcommand: expected usage help in output, got: $OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-no-subcommand"
