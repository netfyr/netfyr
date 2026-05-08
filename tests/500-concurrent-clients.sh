#!/bin/bash
# 500-concurrent-clients.sh
# Integration test: Ten clients connect simultaneously to the daemon and all
# receive correct show responses. Validates the concurrent connection architecture.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/500-concurrent-clients.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 500-concurrent-clients: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 500-concurrent-clients: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

# Enter an unprivileged user+network namespace.
netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
mkdir -p "$POLICY_DIR"

# Create a veth pair so the policy has an interface to configure.
create_veth veth-test0 veth-test1

# Pre-write a static policy.
cat > "$POLICY_DIR/static.yaml" <<'EOF'
kind: policy
name: concurrent-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-test0
  mtu: 1400
EOF

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for the daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 500-concurrent-clients: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 500-concurrent-clients: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# Launch 10 clients in parallel, each querying show.
NUM_CLIENTS=10
RESULTS_DIR="$TMPDIR_TEST/results"
mkdir -p "$RESULTS_DIR"

CLIENT_PIDS=()
export NETFYR_SOCKET_PATH="$SOCKET_PATH"
for i in $(seq 1 $NUM_CLIENTS); do
    (
        output=$("$NETFYR_BIN" show --output json 2>&1)
        echo "$output" > "$RESULTS_DIR/client-$i.json"
    ) &
    CLIENT_PIDS+=($!)
done

# Wait for client processes only (not the daemon).
for pid in "${CLIENT_PIDS[@]}"; do
    wait "$pid" || true
done

# Verify all 10 clients received correct responses.
FAILED=0
for i in $(seq 1 $NUM_CLIENTS); do
    result_file="$RESULTS_DIR/client-$i.json"
    if [[ ! -f "$result_file" ]]; then
        echo "FAIL: 500-concurrent-clients: client $i produced no output file" >&2
        FAILED=1
        continue
    fi
    output=$(cat "$result_file")
    if [[ -z "$output" ]]; then
        echo "FAIL: 500-concurrent-clients: client $i returned empty output" >&2
        FAILED=1
        continue
    fi
    if ! echo "$output" | grep -q '"daemon"'; then
        echo "FAIL: 500-concurrent-clients: client $i response missing daemon object" >&2
        echo "      output: $output" >&2
        FAILED=1
        continue
    fi
    if ! echo "$output" | grep -q '"uptime_seconds"'; then
        echo "FAIL: 500-concurrent-clients: client $i response missing uptime_seconds" >&2
        echo "      output: $output" >&2
        FAILED=1
        continue
    fi
done

if [[ $FAILED -ne 0 ]]; then
    exit 1
fi

# Verify daemon is still alive after concurrent connections.
if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "FAIL: 500-concurrent-clients: daemon crashed during concurrent connections" >&2
    exit 1
fi

echo "PASS: 500-concurrent-clients"
