#!/bin/bash
# 301-apply-dry-run.sh
# Integration test: --dry-run previews changes without modifying kernel state.
# Mapped to spec shell scenario: "Dry-run does not change state in namespace".

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-apply-dry-run: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode: ensure no daemon socket is consulted.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
mtu: 1400
EOF

# Capture exit code without triggering set -e on non-zero exit.
DRY_RUN_EXIT=0
"$NETFYR_BIN" apply --dry-run "$POLICY_FILE" || DRY_RUN_EXIT=$?

# --dry-run returns exit 1 when changes are pending (not applied).
if [[ $DRY_RUN_EXIT -ne 1 ]]; then
    echo "FAIL: 301-apply-dry-run: expected exit code 1 from --dry-run, got $DRY_RUN_EXIT" >&2
    exit 1
fi

# Kernel MTU must remain at the default 1500 — dry-run must not apply changes.
LINK_OUTPUT=$(ip link show veth-test0)
if ! echo "$LINK_OUTPUT" | grep -q "mtu 1500"; then
    echo "FAIL: 301-apply-dry-run: veth-test0 MTU was changed by --dry-run (expected mtu 1500)" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-apply-dry-run"
