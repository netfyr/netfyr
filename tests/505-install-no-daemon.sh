#!/bin/bash
# 505-install-no-daemon.sh
# Verify that --no-daemon skips the netfyr-daemon binary, section 8 man pages,
# and all systemd units.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
REAL_INSTALL_SH="$PROJECT_ROOT/scripts/install.sh"

if [[ ! -f "$REAL_INSTALL_SH" ]]; then
    echo "FAIL: 505-install-no-daemon: scripts/install.sh not found" >&2
    exit 1
fi

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

FAKE_PROJECT="$TMPDIR_TEST/project"
mkdir -p "$FAKE_PROJECT/scripts"
mkdir -p "$FAKE_PROJECT/target/release"
cp "$REAL_INSTALL_SH" "$FAKE_PROJECT/scripts/install.sh"

# Only the main binary — no daemon binary (--no-daemon skips the daemon check).
printf '#!/bin/bash\n[ "${1:-}" = completions ] && [ "${2:-}" = bash ] && echo "# completions"\nexit 0\n' \
    > "$FAKE_PROJECT/target/release/netfyr"
chmod 0755 "$FAKE_PROJECT/target/release/netfyr"

# Symlink the real man/ directory so the script installs sections 1/5/7 and we
# can verify section 8 is absent.
ln -s "$PROJECT_ROOT/man"      "$FAKE_PROJECT/man"

DEST="$TMPDIR_TEST/dest"
SYSTEMDDIR="$TMPDIR_TEST/systemd"
SYSCONFDIR="$TMPDIR_TEST/etc"
mkdir -p "$DEST" "$SYSTEMDDIR" "$SYSCONFDIR"

EXIT_CODE=0
OUTPUT=$(
    SYSTEMDDIR="$SYSTEMDDIR" SYSCONFDIR="$SYSCONFDIR" \
    bash "$FAKE_PROJECT/scripts/install.sh" --prefix "$DEST" --no-daemon 2>&1
) || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 505-install-no-daemon: install.sh failed with exit code $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

failed=0

# netfyr must be installed.
if [[ ! -x "$DEST/bin/netfyr" ]]; then
    echo "FAIL: 505-install-no-daemon: $DEST/bin/netfyr not installed" >&2
    failed=1
fi

# netfyr-daemon must NOT be installed.
if [[ -e "$DEST/bin/netfyr-daemon" ]]; then
    echo "FAIL: 505-install-no-daemon: $DEST/bin/netfyr-daemon should not be installed with --no-daemon" >&2
    failed=1
fi

# Section 8 man pages must NOT be installed.
if find "$DEST/share/man/man8" -maxdepth 1 -name "*.8" 2>/dev/null | grep -q .; then
    echo "FAIL: 505-install-no-daemon: section 8 man pages were installed despite --no-daemon" >&2
    failed=1
fi

# No systemd units must be installed (--no-daemon implies no systemd units).
if find "$SYSTEMDDIR" -maxdepth 1 \( -name "*.service" -o -name "*.socket" \) 2>/dev/null | grep -q .; then
    echo "FAIL: 505-install-no-daemon: systemd units were installed despite --no-daemon" >&2
    failed=1
fi

if [[ $failed -ne 0 ]]; then
    exit 1
fi

echo "PASS: 505-install-no-daemon"
