#!/bin/bash
# 403-daemon-restart.sh
# Integration test: Policies persisted to disk survive a daemon restart.
# Mapped to spec scenario #4 and acceptance criteria:
#   "Daemon loads persisted policies on startup"
#   "End-to-end policy persistence across daemon restart"
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-daemon-restart.sh
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

# Apply a static policy setting mtu=1400 on veth-e2e0.
POLICY_FILE="$TMPDIR_TEST/persist-mtu.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: persist-mtu
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
    echo "FAIL: 403-daemon-restart: initial apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400

# Stop the first daemon instance and start a fresh one with the same POLICY_DIR.
# The new daemon calls PolicyStore::load() and reconcile_and_apply(DaemonStartup).
stop_daemon
start_daemon

# The persisted policy must have been reloaded and re-applied.
assert_mtu veth-e2e0 1400

echo "PASS: 403-daemon-restart"
