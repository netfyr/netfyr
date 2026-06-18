#!/bin/bash
# 505-install-no-completions.sh
# Verify that --no-completions skips bash completion file generation entirely.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
REAL_INSTALL_SH="$PROJECT_ROOT/scripts/install.sh"

if [[ ! -f "$REAL_INSTALL_SH" ]]; then
    echo "FAIL: 505-install-no-completions: scripts/install.sh not found" >&2
    exit 1
fi

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

FAKE_PROJECT="$TMPDIR_TEST/project"
mkdir -p "$FAKE_PROJECT/scripts"
mkdir -p "$FAKE_PROJECT/target/release"
cp "$REAL_INSTALL_SH" "$FAKE_PROJECT/scripts/install.sh"

# Both binaries (daemon defaults to enabled); neither needs to handle completions.
for bin in netfyr netfyr-daemon; do
    printf '#!/bin/bash\nexit 0\n' > "$FAKE_PROJECT/target/release/$bin"
    chmod 0755 "$FAKE_PROJECT/target/release/$bin"
done

# Systemd units are installed by default (daemon + systemd enabled); provide dist/.
ln -s "$PROJECT_ROOT/dist"     "$FAKE_PROJECT/dist"

DEST="$TMPDIR_TEST/dest"
SYSTEMDDIR="$TMPDIR_TEST/systemd"
SYSCONFDIR="$TMPDIR_TEST/etc"
mkdir -p "$DEST" "$SYSTEMDDIR" "$SYSCONFDIR"

EXIT_CODE=0
OUTPUT=$(
    SYSTEMDDIR="$SYSTEMDDIR" SYSCONFDIR="$SYSCONFDIR" \
    bash "$FAKE_PROJECT/scripts/install.sh" --prefix "$DEST" --no-completions 2>&1
) || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 505-install-no-completions: install.sh failed with exit code $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

COMPLETIONDIR="$DEST/share/bash-completion/completions"
if [[ -e "$COMPLETIONDIR/netfyr" ]]; then
    echo "FAIL: 505-install-no-completions: completion file was created despite --no-completions" >&2
    exit 1
fi

echo "PASS: 505-install-no-completions"
