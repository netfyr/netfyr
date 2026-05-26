#!/bin/bash
# 001-makefile-target.sh
# Verify that the Makefile exists and defines the integration-test target with
# the correct behavior as required by spec-001:
#   - Runs "cargo build" first
#   - Discovers tests/[0-9]*.sh scripts
#   - Runs each discovered script
#   - Exits non-zero if any test fails
#
# Usage: bash tests/001-makefile-target.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
MAKEFILE="$PROJECT_ROOT/Makefile"

# Prerequisite: Makefile must exist.
if [[ ! -f "$MAKEFILE" ]]; then
    echo "FAIL: 001-makefile-target: Makefile not found at $MAKEFILE" >&2
    exit 1
fi

failed=0

# The Makefile must declare an integration-test target.
if ! grep -q 'integration-test' "$MAKEFILE"; then
    echo "FAIL: 001-makefile-target: 'integration-test' target not found in Makefile" >&2
    failed=1
fi

# The integration-test target must run "cargo build" before tests.
if ! grep -q 'cargo build' "$MAKEFILE"; then
    echo "FAIL: 001-makefile-target: 'cargo build' not found in Makefile" >&2
    failed=1
fi

# The integration-test target must discover tests/[0-9]*.sh scripts.
# The glob may live in the Makefile itself or in a helper script it invokes
# (e.g. scripts/run-integration-tests.sh).
RUN_TESTS_SCRIPT="$PROJECT_ROOT/scripts/run-integration-tests.sh"
if ! grep -q 'tests/\[0-9\]\*\.sh' "$MAKEFILE" 2>/dev/null; then
    # Check whether the pattern lives in an invoked helper script.
    if [[ -f "$RUN_TESTS_SCRIPT" ]] && grep -q 'tests/\[0-9\]\*\.sh' "$RUN_TESTS_SCRIPT"; then
        : # Pattern found in the helper script — acceptable.
    else
        echo "FAIL: 001-makefile-target: glob 'tests/[0-9]*.sh' not found in Makefile or scripts/run-integration-tests.sh" >&2
        failed=1
    fi
fi

# The Makefile must propagate failure (set failed=1 / exit 1 on failure).
if ! grep -q 'failed' "$MAKEFILE"; then
    echo "FAIL: 001-makefile-target: failure tracking ('failed') not found in Makefile" >&2
    failed=1
fi

# The integration-test target must be declared as .PHONY.
if ! grep -q '\.PHONY' "$MAKEFILE"; then
    echo "FAIL: 001-makefile-target: .PHONY declaration not found in Makefile" >&2
    failed=1
fi

if [[ "$failed" -eq 1 ]]; then
    exit 1
fi

echo "PASS: 001-makefile-target"
