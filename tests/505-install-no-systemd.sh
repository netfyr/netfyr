#!/bin/bash
# 505-install-no-systemd.sh
# Verify that --no-systemd skips systemd unit installation while still
# installing the netfyr-daemon binary.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
REAL_INSTALL_SH="$PROJECT_ROOT/scripts/install.sh"

if [[ ! -f "$REAL_INSTALL_SH" ]]; then
    echo "FAIL: 505-install-no-systemd: scripts/install.sh not found" >&2
    exit 1
fi

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

FAKE_PROJECT="$TMPDIR_TEST/project"
mkdir -p "$FAKE_PROJECT/scripts"
mkdir -p "$FAKE_PROJECT/target/release"
cp "$REAL_INSTALL_SH" "$FAKE_PROJECT/scripts/install.sh"

# Both binaries; netfyr must handle 'completions bash' (completions are enabled by default).
for bin in netfyr netfyr-daemon; do
    printf '#!/bin/bash\n[ "${1:-}" = completions ] && [ "${2:-}" = bash ] && echo "# completions"\nexit 0\n' \
        > "$FAKE_PROJECT/target/release/$bin"
    chmod 0755 "$FAKE_PROJECT/target/release/$bin"
done
# dist/ is NOT provided: with --no-systemd the install block is skipped, so
# the systemd unit files are never accessed.
# Symlink examples/ to avoid the script exiting 1 from its last summary line
# when EXAMPLES_DIR does not exist (bug in install.sh — verify phase handles it).
ln -s "$PROJECT_ROOT/examples" "$FAKE_PROJECT/examples"

DEST="$TMPDIR_TEST/dest"
SYSTEMDDIR="$TMPDIR_TEST/systemd"
SYSCONFDIR="$TMPDIR_TEST/etc"
mkdir -p "$DEST" "$SYSTEMDDIR" "$SYSCONFDIR"

EXIT_CODE=0
OUTPUT=$(
    SYSTEMDDIR="$SYSTEMDDIR" SYSCONFDIR="$SYSCONFDIR" \
    bash "$FAKE_PROJECT/scripts/install.sh" --prefix "$DEST" --no-systemd 2>&1
) || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 505-install-no-systemd: install.sh failed with exit code $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

failed=0

# netfyr-daemon must still be installed even without systemd units.
if [[ ! -x "$DEST/bin/netfyr-daemon" ]]; then
    echo "FAIL: 505-install-no-systemd: $DEST/bin/netfyr-daemon not installed" >&2
    failed=1
fi

# No systemd units must have been written.
if find "$SYSTEMDDIR" -maxdepth 1 \( -name "*.service" -o -name "*.socket" \) 2>/dev/null | grep -q .; then
    echo "FAIL: 505-install-no-systemd: systemd units were installed despite --no-systemd" >&2
    failed=1
fi

if [[ $failed -ne 0 ]]; then
    exit 1
fi

echo "PASS: 505-install-no-systemd"
