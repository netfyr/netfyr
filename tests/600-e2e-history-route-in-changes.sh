#!/bin/bash
# 600-e2e-history-route-in-changes.sh -- End-to-end: netfyr history CHANGES
# column shows non-default routes as count-only ("+N routes") and default
# routes by value ("+dflt via GW").
#
# Scenarios:
#   1. Single non-default route         → "+1 route" (count-only)
#   2. Multiple non-default routes      → "+N routes" (count-only)
#   3. Non-default route removal        → "-N routes" (count-only)
#   4. Default route                    → "+dflt via GW" (by value)
#   5. Default + non-default routes     → "+dflt via GW, +N routes"
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

# ── Scenario 1: Single non-default route → "+1 route" (count-only) ──────────

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

apply_policy "$POLICY_1" "single non-default route"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
assert_contains "$LAST_LINE" "+1 route" \
    "scenario 1: single non-default route must show '+1 route' count-only"
assert_not_contains "$LAST_LINE" "+rt" \
    "scenario 1: must not show individual '+rt' format"

# ── Scenario 2: Multiple non-default routes → "+N routes" (count-only) ──────

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
    - destination: "172.16.0.0/12"
    - destination: "192.168.2.0/24"
    - destination: "10.50.0.0/16"
EOF

apply_policy "$POLICY_2" "multiple non-default routes"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
# 3 new routes added (policy 2 has 3 routes, policy 1 had 1)
# The diff will show +2 routes added and -1 route removed (or just the net change)
# What matters is that the word "route" appears and "+rt" does not.
assert_matches "$LAST_LINE" '[+-][0-9]+ route' \
    "scenario 2: multiple routes must show count-only format (e.g. '+2 routes')"
assert_not_contains "$LAST_LINE" "+rt" \
    "scenario 2: must not show individual '+rt' format"

# ── Scenario 3: Non-default route removal → "-N routes" (count-only) ────────

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
EOF

apply_policy "$POLICY_3" "remove all non-default routes"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
# Should show "-N routes" for the removed routes
assert_matches "$LAST_LINE" '-[0-9]+ route' \
    "scenario 3: removed non-default routes must show count-only format (e.g. '-3 routes')"
assert_not_contains "$LAST_LINE" "-rt" \
    "scenario 3: must not show individual '-rt' format"

# ── Scenario 4: Default route → "+dflt via GW" (by value) ───────────────────

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
  routes:
    - destination: "0.0.0.0/0"
      gateway: "10.99.0.254"
EOF

apply_policy_allow_fail "$POLICY_4"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
assert_contains "$LAST_LINE" "+dflt via 10.99.0.254" \
    "scenario 4: default route must be shown by value '+dflt via 10.99.0.254'"

# ── Scenario 5: Default + non-default → "+dflt via GW, +N routes" ───────────

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
    - destination: "0.0.0.0/0"
      gateway: "10.99.0.254"
    - destination: "10.1.0.0/24"
    - destination: "10.2.0.0/24"
    - destination: "10.3.0.0/24"
EOF

apply_policy_allow_fail "$POLICY_5"

HIST=$(run_history -n 1)
LAST_LINE=$(echo "$HIST" | tail -n 1)
# Default route by value, non-default as count
assert_contains "$LAST_LINE" "+dflt via 10.99.0.254" \
    "scenario 5: default route must be shown by value '+dflt via 10.99.0.254'"
# 3 new non-default routes (10.1, 10.2, 10.3) were added vs previous 0 non-default
assert_matches "$LAST_LINE" '[+][0-9]+ route' \
    "scenario 5: non-default routes must show count-only format"
assert_not_contains "$LAST_LINE" "+rt " \
    "scenario 5: must not show individual '+rt' entries"

echo "PASS: $TEST_NAME"
