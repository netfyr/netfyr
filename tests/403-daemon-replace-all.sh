#!/bin/bash
# 403-daemon-replace-all.sh
# Integration test: Replace-all semantics removes addresses that were present
# in the old policy set but absent from the new one.
# Mapped to spec scenario #3 and acceptance criteria:
#   "End-to-end replace-all semantics"
#   "Applying a new policy set removes the old one"
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/403-daemon-replace-all.sh
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

# ── Phase 1: Apply policy A (mtu=1400, address 10.99.0.1/24) ─────────────────

POLICY_A="$TMPDIR_TEST/policy-a.yaml"
cat > "$POLICY_A" <<'EOF'
kind: policy
name: replace-addr-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1400
  addresses:
    - "10.99.0.1/24"
EOF

APPLY_A_EXIT=0
"$NETFYR_BIN" apply "$POLICY_A" || APPLY_A_EXIT=$?
if [[ $APPLY_A_EXIT -ne 0 ]]; then
    echo "FAIL: 403-daemon-replace-all: first apply exited with code $APPLY_A_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1400
assert_has_address veth-e2e0 "10.99.0.1/24"

# ── Phase 2: Apply policy B (mtu=1300, no addresses) replacing policy A ───────
# Replace-all: the daemon now has only policy B, so the address from policy A
# must be removed from the kernel.

POLICY_B="$TMPDIR_TEST/policy-b.yaml"
cat > "$POLICY_B" <<'EOF'
kind: policy
name: replace-addr-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-e2e0
  mtu: 1300
EOF

APPLY_B_EXIT=0
"$NETFYR_BIN" apply "$POLICY_B" || APPLY_B_EXIT=$?
if [[ $APPLY_B_EXIT -ne 0 ]]; then
    echo "FAIL: 403-daemon-replace-all: second apply exited with code $APPLY_B_EXIT" >&2
    exit 1
fi

assert_mtu veth-e2e0 1300
assert_not_has_address veth-e2e0 "10.99.0.1"

echo "PASS: 403-daemon-replace-all"
