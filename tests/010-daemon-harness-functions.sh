#!/bin/bash
# 010-daemon-harness-functions.sh
# Verify that tests/helpers.sh defines all new helper functions specified in
# SPEC-010: daemon-mode test harness.
#
# Acceptance criteria covered:
#   - All daemon-mode helper functions are defined in helpers.sh
#   - helpers.sh passes bash syntax check
#
# Does not require a network namespace.
# Usage: bash tests/010-daemon-harness-functions.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HELPERS="$SCRIPT_DIR/helpers.sh"

if [[ ! -f "$HELPERS" ]]; then
    echo "FAIL: 010-daemon-harness-functions: tests/helpers.sh does not exist" >&2
    exit 1
fi

failed=0

# New functions introduced by SPEC-010.
required_functions=(
    "require_binaries"
    "daemon_test_setup"
    "_daemon_test_cleanup"
    "setup_journal"
    "start_daemon"
    "stop_daemon"
    "restart_daemon"
    "wait_for_address"
    "wait_for_no_address"
    "setup_dhcp_topology"
)

for fn in "${required_functions[@]}"; do
    if ! grep -qE "^${fn}\s*\(\)" "$HELPERS"; then
        echo "FAIL: 010-daemon-harness-functions: '$fn' not defined in helpers.sh" >&2
        failed=1
    fi
done

# helpers.sh must be a valid bash script.
if ! bash -n "$HELPERS" 2>/dev/null; then
    echo "FAIL: 010-daemon-harness-functions: helpers.sh has bash syntax errors" >&2
    failed=1
fi

if [[ "$failed" -eq 1 ]]; then
    exit 1
fi

echo "PASS: 010-daemon-harness-functions"
