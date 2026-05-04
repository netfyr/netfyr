#!/bin/bash
# 600-e2e-history-route-in-changes.sh -- End-to-end: netfyr history CHANGES
# column shows route destinations explicitly (with "rt" prefix and optional
# "via" gateway) instead of abbreviated "+N route(s)" counts.
#
# Scenarios:
#   1. Single route without gateway  → "+rt DEST"
#   2. Single route with gateway     → "+rt DEST via GW"
#   3. Multiple routes (< 9)         → each shown individually
#   4. Route removal                 → "-rt DEST"
#   5. Many routes (>= 9)            → falls back to "+N routes" count
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-history-route-in-changes.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-route-in-changes: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-history-route-in-changes: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
DAEMON_PID=""
trap 'kill "${DAEMON_PID:-}" 2>/dev/null || true; rm -rf "$TMPDIR_TEST"' EXIT

SOCKET_PATH="$TMPDIR_TEST/netfyr.sock"
POLICY_DIR="$TMPDIR_TEST/policies"
JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$POLICY_DIR" "$JOURNAL_DIR"

create_veth veth-rt0 veth-rt1
add_address veth-rt0 10.99.0.1/24

# Start the daemon.
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_POLICY_DIR="$POLICY_DIR" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_DAEMON_BIN" &
DAEMON_PID=$!

# Poll for daemon socket (up to 5 seconds).
SOCKET_WAIT=0
while [[ ! -S "$SOCKET_PATH" ]]; do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "FAIL: 600-e2e-history-route-in-changes: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-history-route-in-changes: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

TEST_NAME="600-e2e-history-route-in-changes"

apply_policy() {
    local policy_file="$1"
    local label="$2"
    local exit_code=0
    NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
        "$NETFYR_BIN" apply "$policy_file" 2>&1 || exit_code=$?
    if [[ $exit_code -ne 0 ]]; then
        echo "FAIL: $TEST_NAME: apply ($label) exited with code $exit_code" >&2
        exit 1
    fi
}

# Like apply_policy but tolerates failure — the journal entry is still written.
apply_policy_allow_fail() {
    local policy_file="$1"
    NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
        "$NETFYR_BIN" apply "$policy_file" 2>&1 || true
}

run_history() {
    NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    NO_COLOR=1 \
        "$NETFYR_BIN" history "$@" 2>&1
}

assert_contains() {
    local haystack="$1"
    local needle="$2"
    local msg="$3"
    if ! echo "$haystack" | grep -qF -- "$needle"; then
        echo "FAIL: $TEST_NAME: $msg" >&2
        echo "      expected to find: $needle" >&2
        echo "      output: $haystack" >&2
        exit 1
    fi
}

assert_not_contains() {
    local haystack="$1"
    local needle="$2"
    local msg="$3"
    if echo "$haystack" | grep -qF -- "$needle"; then
        echo "FAIL: $TEST_NAME: $msg" >&2
        echo "      expected NOT to find: $needle" >&2
        echo "      output: $haystack" >&2
        exit 1
    fi
}

assert_matches() {
    local haystack="$1"
    local pattern="$2"
    local msg="$3"
    if ! echo "$haystack" | grep -qE "$pattern"; then
        echo "FAIL: $TEST_NAME: $msg" >&2
        echo "      expected to match: $pattern" >&2
        echo "      output: $haystack" >&2
        exit 1
    fi
}

# ── Scenario 1: Single route without gateway → "+rt 10.100.0.0/24" ─────────

POLICY_1="$TMPDIR_TEST/policy-1.yaml"
cat > "$POLICY_1" <<'EOF'
kind: policy
name: rt-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-rt0
  addresses:
    - "10.99.0.1/24"
  routes:
    - destination: "10.100.0.0/24"
EOF

apply_policy "$POLICY_1" "single route without gateway"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
assert_contains "$LAST_LINE" "+rt 10.100.0.0/24" \
    "scenario 1: single route without gateway must show '+rt 10.100.0.0/24'"
assert_not_contains "$LAST_LINE" "+1 route" \
    "scenario 1: must not show abbreviated '+1 route'"

# ── Scenario 2: Route with gateway → "+rt DEST via GW" ─────────────────────
# The daemon has a known issue applying static gateway routes (it appends /32
# to the gateway IP), so kernel apply fails. The journal entry is still written
# with the route diff, which is what we need to test the history format.

POLICY_2="$TMPDIR_TEST/policy-2.yaml"
cat > "$POLICY_2" <<'EOF'
kind: policy
name: rt-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-rt0
  addresses:
    - "10.99.0.1/24"
  routes:
    - destination: "10.200.0.0/16"
      gateway: "10.99.0.2"
EOF

apply_policy_allow_fail "$POLICY_2"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
assert_matches "$LAST_LINE" '\+rt 10\.200\.0\.0/16 via 10\.99\.0\.2' \
    "scenario 2: route with gateway must show '+rt 10.200.0.0/16 via 10.99.0.2'"

# ── Scenario 3: Multiple routes (< 9) → each shown individually ────────────

POLICY_3="$TMPDIR_TEST/policy-3.yaml"
cat > "$POLICY_3" <<'EOF'
kind: policy
name: rt-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-rt0
  addresses:
    - "10.99.0.1/24"
  routes:
    - destination: "172.16.0.0/12"
    - destination: "192.168.2.0/24"
    - destination: "10.50.0.0/16"
EOF

apply_policy "$POLICY_3" "multiple routes"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
assert_contains "$LAST_LINE" "+rt 172.16.0.0/12" \
    "scenario 3: must show '+rt 172.16.0.0/12'"
assert_contains "$LAST_LINE" "+rt 192.168.2.0/24" \
    "scenario 3: must show '+rt 192.168.2.0/24'"
assert_contains "$LAST_LINE" "+rt 10.50.0.0/16" \
    "scenario 3: must show '+rt 10.50.0.0/16'"
assert_not_contains "$LAST_LINE" "+3 routes" \
    "scenario 3: must not show abbreviated '+3 routes'"

# ── Scenario 4: Route removal → "-rt DEST" ─────────────────────────────────

POLICY_4="$TMPDIR_TEST/policy-4.yaml"
cat > "$POLICY_4" <<'EOF'
kind: policy
name: rt-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-rt0
  addresses:
    - "10.99.0.1/24"
EOF

apply_policy "$POLICY_4" "remove routes"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
assert_contains "$LAST_LINE" "-rt 172.16.0.0/12" \
    "scenario 4: removal must show '-rt 172.16.0.0/12'"
assert_contains "$LAST_LINE" "-rt 192.168.2.0/24" \
    "scenario 4: removal must show '-rt 192.168.2.0/24'"
assert_contains "$LAST_LINE" "-rt 10.50.0.0/16" \
    "scenario 4: removal must show '-rt 10.50.0.0/16'"

# ── Scenario 5: Many routes (>= 9) → falls back to "+N routes" count ───────

POLICY_5="$TMPDIR_TEST/policy-5.yaml"
cat > "$POLICY_5" <<'EOF'
kind: policy
name: rt-test
factory: static
priority: 100
state:
  type: ethernet
  name: veth-rt0
  addresses:
    - "10.99.0.1/24"
  routes:
    - destination: "10.1.0.0/24"
    - destination: "10.2.0.0/24"
    - destination: "10.3.0.0/24"
    - destination: "10.4.0.0/24"
    - destination: "10.5.0.0/24"
    - destination: "10.6.0.0/24"
    - destination: "10.7.0.0/24"
    - destination: "10.8.0.0/24"
    - destination: "10.9.0.0/24"
EOF

apply_policy "$POLICY_5" "many routes (9)"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
assert_contains "$LAST_LINE" "+9 routes" \
    "scenario 5: 9 routes must fall back to '+9 routes' count"
assert_not_contains "$LAST_LINE" "+rt " \
    "scenario 5: must not show individual '+rt' entries"

echo "PASS: $TEST_NAME"
