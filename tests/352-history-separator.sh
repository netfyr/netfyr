#!/bin/bash
# 352-history-separator.sh -- History shows daemon-restart separator between sessions.
#
# Spec test 26b: the "──── daemon restart ────" line appears after a daemon-startup entry.
#
# Requires: unshare, ip (iproute2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

netns_setup "$@"
require_binaries
daemon_test_setup
setup_journal

create_veth veth-e2e0 veth-e2e1
start_daemon

# Apply policy in the first daemon session.
POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-separator
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-separator: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Restart the daemon (creates a new daemon-startup journal entry).
restart_daemon

# Apply policy in the second daemon session.
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: e2e-separator
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1300
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 352-history-separator: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Run history and verify separator and daemon-startup trigger.
HISTORY_OUTPUT=$(NO_COLOR=1 "$NETFYR_BIN" history -n 10 2>&1)

if ! echo "$HISTORY_OUTPUT" | grep -qF "daemon restart"; then
    echo "FAIL: 352-history-separator: output does not contain 'daemon restart' separator" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

if ! echo "$HISTORY_OUTPUT" | grep -qF "daemon-startup"; then
    echo "FAIL: 352-history-separator: output does not contain 'daemon-startup' trigger" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

# Verify separator appears after the daemon-startup row (not above the oldest entry).
LINES=$(echo "$HISTORY_OUTPUT" | grep -n ".")
STARTUP_LINE=$(echo "$HISTORY_OUTPUT" | grep -n "daemon-startup" | head -n 1 | cut -d: -f1)
SEPARATOR_LINE=$(echo "$HISTORY_OUTPUT" | grep -n "daemon restart" | head -n 1 | cut -d: -f1)

if [[ -z "$STARTUP_LINE" || -z "$SEPARATOR_LINE" ]]; then
    echo "FAIL: 352-history-separator: could not locate startup or separator line numbers" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

if [[ "$SEPARATOR_LINE" -ne $((STARTUP_LINE + 1)) ]]; then
    echo "FAIL: 352-history-separator: separator not immediately after daemon-startup row" >&2
    echo "      daemon-startup at line $STARTUP_LINE, separator at line $SEPARATOR_LINE" >&2
    echo "      output: $HISTORY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 352-history-separator"
