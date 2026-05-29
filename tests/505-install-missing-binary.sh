#!/bin/bash
# 505-install-missing-binary.sh
# Verify that install.sh exits with status 1 and the error message mentions
# "cargo build" when the required netfyr binary is absent from the build directory.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

REAL_INSTALL_SH="$SCRIPT_DIR/../scripts/install.sh"

if [[ ! -f "$REAL_INSTALL_SH" ]]; then
    echo "FAIL: 505-install-missing-binary: scripts/install.sh not found" >&2
    exit 1
fi

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

# Fake project: install script + empty build directory (no binaries).
mkdir -p "$TMPDIR_TEST/project/scripts"
mkdir -p "$TMPDIR_TEST/project/target/release"
cp "$REAL_INSTALL_SH" "$TMPDIR_TEST/project/scripts/install.sh"

DEST="$TMPDIR_TEST/dest"
SYSTEMDDIR="$TMPDIR_TEST/systemd"
SYSCONFDIR="$TMPDIR_TEST/etc"
mkdir -p "$DEST" "$SYSTEMDDIR" "$SYSCONFDIR"

EXIT_CODE=0
ERROR_OUTPUT=$(
    SYSTEMDDIR="$SYSTEMDDIR" SYSCONFDIR="$SYSCONFDIR" \
    bash "$TMPDIR_TEST/project/scripts/install.sh" --prefix "$DEST" 2>&1
) || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 1 ]]; then
    echo "FAIL: 505-install-missing-binary: expected exit status 1, got $EXIT_CODE" >&2
    echo "      output: $ERROR_OUTPUT" >&2
    exit 1
fi

if ! echo "$ERROR_OUTPUT" | grep -q "cargo build"; then
    echo "FAIL: 505-install-missing-binary: error output does not mention 'cargo build'" >&2
    echo "      output: $ERROR_OUTPUT" >&2
    exit 1
fi

echo "PASS: 505-install-missing-binary"
