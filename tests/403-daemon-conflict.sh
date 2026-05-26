#!/bin/bash
# 403-daemon-conflict.sh
# Integration test: Two policies competing for the same field at the same
# priority produce a conflict warning on stderr and exit code 1.
# Mapped to spec scenario #5 and acceptance criteria:
#   "End-to-end conflict detection"
#   "Conflicting policies produce a warning"
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-daemon-conflict.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

create_veth veth-e2e0 veth-e2e1

start_daemon

# Write a single YAML file with two policies (multi-document) that both target
# veth-e2e0's mtu at the same priority — this guarantees a conflict.
POLICY_FILE="$TMPDIR_TEST/conflict-policies.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: conflict-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
---
kind: policy
name: conflict-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1300
EOF

STDERR_FILE="$TMPDIR_TEST/apply-stderr"
APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" 2>"$STDERR_FILE" || APPLY_EXIT=$?

# Conflicting policies cause exit code 1 (not 0 success, not 2 total failure).
if [[ $APPLY_EXIT -ne 1 ]]; then
    echo "FAIL: 403-daemon-conflict: expected exit code 1, got $APPLY_EXIT" >&2
    echo "      stderr: $(cat "$STDERR_FILE")" >&2
    exit 1
fi

# The conflict warning must mention "conflict" (case-insensitive).
if ! grep -qi "conflict" "$STDERR_FILE"; then
    echo "FAIL: 403-daemon-conflict: stderr does not contain 'conflict'" >&2
    echo "      stderr: $(cat "$STDERR_FILE")" >&2
    exit 1
fi

# The conflict warning must mention the conflicting field "mtu".
if ! grep -qi "mtu" "$STDERR_FILE"; then
    echo "FAIL: 403-daemon-conflict: stderr does not mention the conflicting field 'mtu'" >&2
    echo "      stderr: $(cat "$STDERR_FILE")" >&2
    exit 1
fi

echo "PASS: 403-daemon-conflict"
