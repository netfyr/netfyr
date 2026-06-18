#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

PREFIX="${PREFIX:-/usr/local}"
BINDIR="${BINDIR:-$PREFIX/bin}"
MANDIR="${MANDIR:-$PREFIX/share/man}"
COMPLETIONDIR="${COMPLETIONDIR:-$PREFIX/share/bash-completion/completions}"
SYSTEMDDIR="${SYSTEMDDIR:-/usr/lib/systemd/system}"
SYSCONFDIR="${SYSCONFDIR:-/etc}"
DOCDIR="${DOCDIR:-$PREFIX/share/doc/netfyr}"

BUILD_PROFILE="release"
BUILD_DIR="$PROJECT_DIR/target/$BUILD_PROFILE"
MAN_DIR="$PROJECT_DIR/man"
DIST_DIR="$PROJECT_DIR/dist/systemd"
EXAMPLES_DIR="$PROJECT_DIR/examples/policies"

usage() {
    cat <<EOF
Usage: $0 [--prefix PREFIX] [--debug] [--no-daemon] [--no-completions] [--no-systemd]

Install netfyr binaries, man pages, and supporting files.

Requires a prior build:  cargo build [--release]

Options:
  --prefix DIR       Installation prefix (default: /usr/local)
  --debug            Install from target/debug instead of target/release
  --no-daemon        Skip installing netfyr-daemon and its systemd units
  --no-completions   Skip installing bash completions
  --no-systemd       Skip installing systemd unit files
  -h, --help         Show this help message

Environment variables (override individual directories):
  PREFIX, BINDIR, MANDIR, COMPLETIONDIR, SYSTEMDDIR, SYSCONFDIR, DOCDIR
EOF
    exit 0
}

INSTALL_DAEMON=1
INSTALL_COMPLETIONS=1
INSTALL_SYSTEMD=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            PREFIX="$2"
            BINDIR="$PREFIX/bin"
            MANDIR="$PREFIX/share/man"
            COMPLETIONDIR="$PREFIX/share/bash-completion/completions"
            DOCDIR="$PREFIX/share/doc/netfyr"
            shift 2
            ;;
        --debug)
            BUILD_PROFILE="debug"
            BUILD_DIR="$PROJECT_DIR/target/$BUILD_PROFILE"
            shift
            ;;
        --no-daemon)       INSTALL_DAEMON=0;      shift ;;
        --no-completions)  INSTALL_COMPLETIONS=0;  shift ;;
        --no-systemd)      INSTALL_SYSTEMD=0;      shift ;;
        -h|--help)         usage ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

if [[ $BUILD_PROFILE == release ]]; then
    BUILD_HINT="cargo build --release"
else
    BUILD_HINT="cargo build"
fi

if [[ ! -x "$BUILD_DIR/netfyr" ]]; then
    echo "error: $BUILD_DIR/netfyr not found — run '$BUILD_HINT' first" >&2
    exit 1
fi

if [[ $INSTALL_DAEMON -eq 1 && ! -x "$BUILD_DIR/netfyr-daemon" ]]; then
    echo "error: $BUILD_DIR/netfyr-daemon not found — run '$BUILD_HINT' first" >&2
    exit 1
fi

echo "Installing netfyr to $PREFIX ..."

install -Dpm 0755 "$BUILD_DIR/netfyr" "$BINDIR/netfyr"

if [[ $INSTALL_DAEMON -eq 1 ]]; then
    install -Dpm 0755 "$BUILD_DIR/netfyr-daemon" "$BINDIR/netfyr-daemon"
fi

if [[ -d "$MAN_DIR" ]]; then
    install -d "$MANDIR/man1" "$MANDIR/man5" "$MANDIR/man7" "$MANDIR/man8"
    for f in "$MAN_DIR"/*.1; do
        [[ -f "$f" ]] && install -pm 0644 "$f" "$MANDIR/man1/"
    done
    for f in "$MAN_DIR"/*.5; do
        [[ -f "$f" ]] && install -pm 0644 "$f" "$MANDIR/man5/"
    done
    for f in "$MAN_DIR"/*.7; do
        [[ -f "$f" ]] && install -pm 0644 "$f" "$MANDIR/man7/"
    done
    if [[ $INSTALL_DAEMON -eq 1 ]]; then
        for f in "$MAN_DIR"/*.8; do
            [[ -f "$f" ]] && install -pm 0644 "$f" "$MANDIR/man8/"
        done
    fi
fi

if [[ $INSTALL_COMPLETIONS -eq 1 ]]; then
    install -d "$COMPLETIONDIR"
    "$BUILD_DIR/netfyr" completions bash > "$COMPLETIONDIR/netfyr"
fi

if [[ $INSTALL_DAEMON -eq 1 && $INSTALL_SYSTEMD -eq 1 ]]; then
    install -Dpm 0644 "$DIST_DIR/netfyr.service" "$SYSTEMDDIR/netfyr.service"
    install -Dpm 0644 "$DIST_DIR/netfyr.socket" "$SYSTEMDDIR/netfyr.socket"
fi

install -d "$SYSCONFDIR/netfyr/policies"

if [[ -d "$EXAMPLES_DIR" ]]; then
    install -d "$DOCDIR/examples/policies"
    install -pm 0644 "$EXAMPLES_DIR"/*.yaml "$DOCDIR/examples/policies/"
fi

echo "Done."
echo ""
echo "Installed:"
echo "  netfyr         -> $BINDIR/netfyr"
if [[ $INSTALL_DAEMON -eq 1 ]]; then
    echo "  netfyr-daemon  -> $BINDIR/netfyr-daemon"
fi
if [[ -d "$MAN_DIR" ]]; then
    echo "  man pages      -> $MANDIR/"
fi
if [[ $INSTALL_COMPLETIONS -eq 1 ]]; then
    echo "  completions    -> $COMPLETIONDIR/netfyr"
fi
if [[ $INSTALL_DAEMON -eq 1 && $INSTALL_SYSTEMD -eq 1 ]]; then
    echo "  systemd units  -> $SYSTEMDDIR/"
fi
echo "  config dir     -> $SYSCONFDIR/netfyr/"
if [[ -d "$EXAMPLES_DIR" ]]; then
    echo "  examples       -> $DOCDIR/examples/policies/"
fi
