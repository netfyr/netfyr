#!/bin/bash
# 301-daemon-apply-directory.sh -- Daemon mode: applying a directory of policies
# configures multiple interfaces.
#
# Scenario 7: Creates two veth pairs, starts daemon, writes two policy files
# in a directory, runs netfyr apply $DIR/, verifies both interfaces are
# configured correctly.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-apply-directory.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

create_veth veth-a0 veth-a1
create_veth veth-b0 veth-b1

start_daemon

# Write two policy files in a dedicated directory.
APPLY_DIR="$TMPDIR_TEST/apply-policies"
mkdir -p "$APPLY_DIR"

cat > "$APPLY_DIR/veth-a0.yaml" <<'EOF'
kind: policy
name: apply-dir-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-a0
  mtu: 1400
EOF

cat > "$APPLY_DIR/veth-b0.yaml" <<'EOF'
kind: policy
name: apply-dir-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-b0
  mtu: 1300
EOF

APPLY_EXIT=0
"$NETFYR_BIN" apply "$APPLY_DIR/" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-apply-directory: netfyr apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify both interfaces have the expected MTU values.
assert_mtu veth-a0 1400
assert_mtu veth-b0 1300

echo "PASS: 301-daemon-apply-directory"
