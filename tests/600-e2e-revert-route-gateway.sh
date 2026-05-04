#!/bin/bash
# 600-e2e-revert-route-gateway.sh -- Revert dry-run must not add /32 suffix to route gateways.
#
# Bug 005: reverting to a journal entry whose state_after matches the current
# system state should produce "No changes needed". A bare gateway IP
# (e.g. 10.99.0.1) must not gain a spurious /32 CIDR suffix during the
# round-trip through the journal.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/600-e2e-revert-route-gateway.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"
NETFYR_DAEMON_BIN="${NETFYR_DAEMON_BIN:-$SCRIPT_DIR/../target/debug/netfyr-daemon}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-revert-route-gateway: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if [[ ! -x "$NETFYR_DAEMON_BIN" ]]; then
    echo "FAIL: 600-e2e-revert-route-gateway: netfyr-daemon binary not found at $NETFYR_DAEMON_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-revert-route-gateway: 'jq' not found; install jq to run this test" >&2
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

create_veth veth-e2e0 veth-e2e1
add_address veth-e2e0 10.99.0.1/24

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
        echo "FAIL: 600-e2e-revert-route-gateway: daemon exited before socket appeared" >&2
        exit 1
    fi
    if (( SOCKET_WAIT >= 50 )); then
        echo "FAIL: 600-e2e-revert-route-gateway: daemon socket did not appear within 5 seconds" >&2
        exit 1
    fi
    sleep 0.1
    (( SOCKET_WAIT++ )) || true
done

# ── Apply a policy with addresses only (no routes) to start managing the iface

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-revert-route-gw
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  addresses:
    - "10.99.0.1/24"
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$SOCKET_PATH" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-revert-route-gateway: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# ── Add a route externally (gateway = bare IP, no CIDR) ──────────────────────

ip route add 10.100.0.0/24 via 10.99.0.2 dev veth-e2e0

# Wait for the daemon to detect the external change and journal it.
sleep 1

EC_COUNT=$(jq -rs '[.[] | select(.trigger.type == "external_change")] | length' \
    "$JOURNAL_DIR/current.ndjson")
if [[ "$EC_COUNT" -lt 1 ]]; then
    echo "FAIL: 600-e2e-revert-route-gateway: expected >= 1 external_change entry, got $EC_COUNT" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Get the seq of the latest journal entry (the one that recorded the route).
SEQ=$(jq -rs 'last | .seq' "$JOURNAL_DIR/current.ndjson")
if [[ -z "$SEQ" || "$SEQ" == "null" ]]; then
    echo "FAIL: 600-e2e-revert-route-gateway: could not find latest journal entry" >&2
    cat "$JOURNAL_DIR/current.ndjson" >&2
    exit 1
fi

# Verify the journal recorded the gateway without /32 suffix.
GW_IN_JOURNAL=$(jq -rs 'last | .state_after.entities[].fields.routes // empty | .[]? | .gateway // empty' \
    "$JOURNAL_DIR/current.ndjson" | head -1)
if echo "$GW_IN_JOURNAL" | grep -q "/32"; then
    echo "FAIL: 600-e2e-revert-route-gateway: journal gateway has /32 suffix: $GW_IN_JOURNAL" >&2
    exit 1
fi

# ── Dry-run revert to the latest entry (no changes expected) ─────────────────
# The system state has not changed since the journal entry was written, so
# revert --dry-run should report "No changes needed". Bug 005 causes the
# gateway to gain a /32 suffix during the journal round-trip, producing a
# phantom diff.

DRY_RUN_EXIT=0
DRY_RUN_OUTPUT=$(NETFYR_SOCKET_PATH="$SOCKET_PATH" \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" revert "$SEQ" --dry-run 2>&1) || DRY_RUN_EXIT=$?

if [[ $DRY_RUN_EXIT -ne 0 ]]; then
    echo "FAIL: 600-e2e-revert-route-gateway: dry-run to same state should exit 0, got $DRY_RUN_EXIT" >&2
    echo "      output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

if ! echo "$DRY_RUN_OUTPUT" | grep -qi "no changes"; then
    echo "FAIL: 600-e2e-revert-route-gateway: dry-run output should say 'No changes', got:" >&2
    echo "      $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# The output must NOT contain /32 (the spurious CIDR suffix on gateways).
if echo "$DRY_RUN_OUTPUT" | grep -q "/32"; then
    echo "FAIL: 600-e2e-revert-route-gateway: dry-run output contains spurious /32 suffix" >&2
    echo "      $DRY_RUN_OUTPUT" >&2
    exit 1
fi

echo "PASS: 600-e2e-revert-route-gateway"
