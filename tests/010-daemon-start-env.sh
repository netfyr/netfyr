#!/bin/bash
# 010-daemon-start-env.sh
# Integration test: start_daemon passes extra KEY=VALUE pairs to the daemon
# process environment.
#
# Acceptance criteria covered:
#   - start_daemon RUST_LOG=netfyr_daemon=debug runs daemon with that env var set
#
# Requires: unshare, ip (iproute2), netfyr-daemon binary
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/010-daemon-start-env.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

# Start daemon with an extra environment variable.
start_daemon RUST_LOG=netfyr_daemon=debug

# Read the daemon's environment from /proc to verify the extra var was passed.
# /proc/PID/environ contains null-byte-separated KEY=VALUE pairs.
DAEMON_ENV_FILE="/proc/$DAEMON_PID/environ"

if [[ ! -r "$DAEMON_ENV_FILE" ]]; then
    echo "FAIL: 010-daemon-start-env: cannot read $DAEMON_ENV_FILE" >&2
    exit 1
fi

DAEMON_ENV=$(tr '\0' '\n' < "$DAEMON_ENV_FILE" 2>/dev/null) || {
    echo "FAIL: 010-daemon-start-env: failed to parse daemon environment from $DAEMON_ENV_FILE" >&2
    exit 1
}

if ! echo "$DAEMON_ENV" | grep -q "^RUST_LOG=netfyr_daemon=debug$"; then
    echo "FAIL: 010-daemon-start-env: RUST_LOG=netfyr_daemon=debug not found in daemon environment" >&2
    echo "      Daemon env (RUST_LOG entries):" >&2
    echo "$DAEMON_ENV" | grep "^RUST_LOG" >&2 || echo "      (none)" >&2
    exit 1
fi

# Also verify that NETFYR_SOCKET_PATH was passed (baseline sanity check).
if ! echo "$DAEMON_ENV" | grep -q "^NETFYR_SOCKET_PATH="; then
    echo "FAIL: 010-daemon-start-env: NETFYR_SOCKET_PATH not found in daemon environment" >&2
    exit 1
fi

# Also verify that NETFYR_POLICY_DIR was passed.
if ! echo "$DAEMON_ENV" | grep -q "^NETFYR_POLICY_DIR="; then
    echo "FAIL: 010-daemon-start-env: NETFYR_POLICY_DIR not found in daemon environment" >&2
    exit 1
fi

echo "PASS: 010-daemon-start-env"
