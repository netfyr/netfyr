#!/bin/bash
# 354-revert-addr.sh -- Revert handles address restoration correctly.
#
# Spec test 25: netfyr revert restores a previous address set.
# Verifies that the original addresses are restored and the replaced address removed.
#
# Requires: unshare, ip (iproute2), jq
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   bash tests/354-revert-addr.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 354-revert-addr: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 354-revert-addr: 'jq' not found; install jq to run this test" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

JOURNAL_DIR="$TMPDIR_TEST/journal"
# Point socket at a nonexistent path to force daemon-free mode.
FAKE_SOCKET="$TMPDIR_TEST/no-daemon.sock"
mkdir -p "$JOURNAL_DIR"

create_veth veth-e2e0 veth-e2e1

# ── Apply policy A: two addresses ────────────────────────────────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: e2e-revert-addr-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  addresses:
    - "10.99.0.1/24"
    - "10.99.0.2/24"
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_A" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 354-revert-addr: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Confirm seq=1 exists with two addresses.
SEQ1=$(jq -rs '.[0].seq' "$JOURNAL_DIR/current.ndjson")
if [[ "$SEQ1" != "1" ]]; then
    echo "FAIL: 354-revert-addr: expected first journal entry to have seq=1, got $SEQ1" >&2
    exit 1
fi

# ── Apply policy B: one different address ─────────────────────────────────────

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: e2e-revert-addr-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  addresses:
    - "10.99.0.3/24"
EOF

APPLY_EXIT=0
NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" apply "$POLICY_B" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 354-revert-addr: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify veth-e2e0 has only 10.99.0.3/24.
assert_has_address veth-e2e0 "10.99.0.3"
assert_not_has_address veth-e2e0 "10.99.0.1"
assert_not_has_address veth-e2e0 "10.99.0.2"

# ── Run revert to entry #1 ───────────────────────────────────────────────────

REVERT_EXIT=0
NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" revert 1 || REVERT_EXIT=$?
if [[ $REVERT_EXIT -ne 0 ]]; then
    echo "FAIL: 354-revert-addr: revert exited with code $REVERT_EXIT" >&2
    ip addr show dev veth-e2e0 >&2 || true
    exit 1
fi

# AC: veth-e2e0 has addresses 10.99.0.1/24 and 10.99.0.2/24.
assert_has_address veth-e2e0 "10.99.0.1"
assert_has_address veth-e2e0 "10.99.0.2"

# AC: veth-e2e0 does not have address 10.99.0.3/24.
assert_not_has_address veth-e2e0 "10.99.0.3"

# AC: netfyr history -n 1 shows address changes by value.
HISTORY_OUTPUT=$(NO_COLOR=1 NETFYR_SOCKET_PATH="$FAKE_SOCKET" \
NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 1 2>&1)

# The CHANGES column should show added addresses (10.99.0.1, 10.99.0.2).
if ! echo "$HISTORY_OUTPUT" | grep -qF "10.99.0.1"; then
    echo "FAIL: 354-revert-addr: CHANGES column should show restored address 10.99.0.1" >&2
    echo "      history output: $HISTORY_OUTPUT" >&2
    exit 1
fi
if ! echo "$HISTORY_OUTPUT" | grep -qF "10.99.0.2"; then
    echo "FAIL: 354-revert-addr: CHANGES column should show restored address 10.99.0.2" >&2
    echo "      history output: $HISTORY_OUTPUT" >&2
    exit 1
fi
# The CHANGES column should show removed address (10.99.0.3).
if ! echo "$HISTORY_OUTPUT" | grep -qF "10.99.0.3"; then
    echo "FAIL: 354-revert-addr: CHANGES column should show removed address 10.99.0.3" >&2
    echo "      history output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 354-revert-addr"
