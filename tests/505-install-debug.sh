#!/bin/bash
# 505-install-debug.sh
# Verify that install.sh installs the netfyr binary from target/debug when
# the --debug flag is given.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
REAL_INSTALL_SH="$PROJECT_ROOT/scripts/install.sh"

if [[ ! -f "$REAL_INSTALL_SH" ]]; then
    echo "FAIL: 505-install-debug: scripts/install.sh not found" >&2
    exit 1
fi

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

FAKE_PROJECT="$TMPDIR_TEST/project"
mkdir -p "$FAKE_PROJECT/scripts"
mkdir -p "$FAKE_PROJECT/target/debug"
cp "$REAL_INSTALL_SH" "$FAKE_PROJECT/scripts/install.sh"

# Fake debug binaries; netfyr supports 'completions bash' for the default install.
for bin in netfyr netfyr-daemon; do
    printf '#!/bin/bash\n[ "${1:-}" = completions ] && [ "${2:-}" = bash ] && echo "# completions"\nexit 0\n' \
        > "$FAKE_PROJECT/target/debug/$bin"
    chmod 0755 "$FAKE_PROJECT/target/debug/$bin"
done

# Symlink dist/ so systemd unit files are available (daemon+systemd both default to enabled).
ln -s "$PROJECT_ROOT/dist"     "$FAKE_PROJECT/dist"

DEST="$TMPDIR_TEST/dest"
SYSTEMDDIR="$TMPDIR_TEST/systemd"
SYSCONFDIR="$TMPDIR_TEST/etc"
mkdir -p "$DEST" "$SYSTEMDDIR" "$SYSCONFDIR"

EXIT_CODE=0
OUTPUT=$(
    SYSTEMDDIR="$SYSTEMDDIR" SYSCONFDIR="$SYSCONFDIR" \
    bash "$FAKE_PROJECT/scripts/install.sh" --prefix "$DEST" --debug 2>&1
) || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 505-install-debug: install.sh failed with exit code $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

if [[ ! -x "$DEST/bin/netfyr" ]]; then
    echo "FAIL: 505-install-debug: $DEST/bin/netfyr does not exist or is not executable" >&2
    exit 1
fi

echo "PASS: 505-install-debug"
