#!/bin/bash
# 505-install-default.sh
# Verify that a default installation places all artifacts in the correct locations:
# binaries, man pages (sorted by section), bash completions, systemd units,
# configuration directory, and example policies.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
REAL_INSTALL_SH="$PROJECT_ROOT/scripts/install.sh"

if [[ ! -f "$REAL_INSTALL_SH" ]]; then
    echo "FAIL: 505-install-default: scripts/install.sh not found" >&2
    exit 1
fi

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

# Build a fake project tree: install script + release binaries + content dirs.
FAKE_PROJECT="$TMPDIR_TEST/project"
mkdir -p "$FAKE_PROJECT/scripts"
mkdir -p "$FAKE_PROJECT/target/release"
cp "$REAL_INSTALL_SH" "$FAKE_PROJECT/scripts/install.sh"

# Minimal fake binaries that handle 'netfyr completions bash'.
for bin in netfyr netfyr-daemon; do
    printf '#!/bin/bash\n[ "${1:-}" = completions ] && [ "${2:-}" = bash ] && echo "# completions"\nexit 0\n' \
        > "$FAKE_PROJECT/target/release/$bin"
    chmod 0755 "$FAKE_PROJECT/target/release/$bin"
done

# Symlink real content so the install script finds actual files to copy.
ln -s "$PROJECT_ROOT/man"      "$FAKE_PROJECT/man"
ln -s "$PROJECT_ROOT/dist"     "$FAKE_PROJECT/dist"
ln -s "$PROJECT_ROOT/examples" "$FAKE_PROJECT/examples"

DEST="$TMPDIR_TEST/dest"
SYSTEMDDIR="$TMPDIR_TEST/systemd"
SYSCONFDIR="$TMPDIR_TEST/etc"
mkdir -p "$DEST" "$SYSTEMDDIR" "$SYSCONFDIR"

EXIT_CODE=0
OUTPUT=$(
    SYSTEMDDIR="$SYSTEMDDIR" SYSCONFDIR="$SYSCONFDIR" \
    bash "$FAKE_PROJECT/scripts/install.sh" --prefix "$DEST" 2>&1
) || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 505-install-default: install.sh failed with exit code $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

failed=0

check() {
    local desc="$1" path="$2" kind="$3"
    case "$kind" in
        file)
            if [[ ! -f "$path" ]]; then
                echo "FAIL: 505-install-default: $desc not found: $path" >&2
                failed=1
            fi
            ;;
        exec)
            if [[ ! -x "$path" ]]; then
                echo "FAIL: 505-install-default: $desc not executable: $path" >&2
                failed=1
            fi
            ;;
        dir)
            if [[ ! -d "$path" ]]; then
                echo "FAIL: 505-install-default: $desc directory missing: $path" >&2
                failed=1
            fi
            ;;
    esac
}

# Binaries must exist and be executable.
check "netfyr binary"        "$DEST/bin/netfyr"        exec
check "netfyr-daemon binary" "$DEST/bin/netfyr-daemon" exec

# Man pages sorted into the correct sections.
check "man1/netfyr.1"         "$DEST/share/man/man1/netfyr.1"         file
check "man1/netfyr-apply.1"   "$DEST/share/man/man1/netfyr-apply.1"   file
check "man5/netfyr.yaml.5"    "$DEST/share/man/man5/netfyr.yaml.5"    file
check "man7/netfyr-examples.7" "$DEST/share/man/man7/netfyr-examples.7" file
check "man8/netfyr-daemon.8"  "$DEST/share/man/man8/netfyr-daemon.8"  file

# Bash completions.
check "bash completions" "$DEST/share/bash-completion/completions/netfyr" file

# Systemd units installed to the overridden SYSTEMDDIR.
check "netfyr.service" "$SYSTEMDDIR/netfyr.service" file
check "netfyr.socket"  "$SYSTEMDDIR/netfyr.socket"  file

# Configuration directory.
check "sysconfdir/netfyr/policies" "$SYSCONFDIR/netfyr/policies" dir

# Example policies.
check "example bare-ethernet.yaml" \
    "$DEST/share/doc/netfyr/examples/policies/bare-ethernet.yaml" file

if [[ $failed -ne 0 ]]; then
    exit 1
fi

echo "PASS: 505-install-default"
