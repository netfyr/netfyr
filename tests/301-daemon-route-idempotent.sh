#!/bin/bash
# 301-daemon-route-idempotent.sh -- Daemon mode: re-applying the same route
# policy is idempotent (no errors, no route deletion, no spurious diff).
#
# Scenario 60: Creates veth pair, starts daemon, applies a policy with an
# address and a default route. Verifies the route is present. Dry-run shows
# no changes (exit 0). Second apply is clean (exit 0). Route still present.
#
# Requires: unshare, ip (iproute2)
# Usage:
#   NETFYR_BIN=./target/debug/netfyr \
#   NETFYR_DAEMON_BIN=./target/debug/netfyr-daemon \
#   bash tests/301-daemon-route-idempotent.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

require_binaries
netns_setup "$@"

# ---------- Inside the namespace ----------

daemon_test_setup

create_veth veth-rt0 veth-rt1

start_daemon

POLICY_FILE="$TMPDIR_TEST/policy.yaml"
cat > "$POLICY_FILE" <<'EOF'
kind: policy
name: route-idempotent
factory: static
priority: 100
state:
  type: ethernet
  name: veth-rt0
  enabled: true
  addresses:
    - "10.99.0.50/24"
  routes:
    - destination: "0.0.0.0/0"
      gateway: "10.99.0.254"
EOF

# ── First apply ───────────────────────────────────────────────────────────────

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-route-idempotent: first apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# Verify the route exists after first apply.
ROUTE_OUTPUT=$(ip route 2>&1)
if ! echo "$ROUTE_OUTPUT" | grep -q "default via 10.99.0.254"; then
    echo "FAIL: 301-daemon-route-idempotent: default route not present after first apply" >&2
    echo "      ip route output: $ROUTE_OUTPUT" >&2
    exit 1
fi

# ── Dry-run must report no changes ────────────────────────────────────────────

DRY_RUN_EXIT=0
DRY_RUN_OUTPUT=$("$NETFYR_BIN" apply --dry-run "$POLICY_FILE" 2>&1) || DRY_RUN_EXIT=$?

if [[ $DRY_RUN_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-route-idempotent: dry-run reports changes on second apply (exit=$DRY_RUN_EXIT)" >&2
    echo "      dry-run output: $DRY_RUN_OUTPUT" >&2
    exit 1
fi

# ── Second apply must be a no-op ──────────────────────────────────────────────

APPLY_EXIT=0
"$NETFYR_BIN" apply "$POLICY_FILE" || APPLY_EXIT=$?
if [[ $APPLY_EXIT -ne 0 ]]; then
    echo "FAIL: 301-daemon-route-idempotent: second apply exited with code $APPLY_EXIT" >&2
    exit 1
fi

# The default route must still be present after the second apply.
ROUTE_OUTPUT=$(ip route 2>&1)
if ! echo "$ROUTE_OUTPUT" | grep -q "default via 10.99.0.254"; then
    echo "FAIL: 301-daemon-route-idempotent: default route disappeared after second apply" >&2
    echo "      ip route output: $ROUTE_OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-daemon-route-idempotent"
