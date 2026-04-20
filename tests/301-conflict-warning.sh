#!/bin/bash
# 301-conflict-warning.sh
# AC: "Conflicts are reported as warnings"
#
# When two policies both set the same field on the same entity at the same
# priority but with different values, a conflict warning must appear in the
# output and the conflicting field must not be applied. Other non-conflicting
# fields must be applied. Exit code must be 1.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-conflict-warning: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_DIR=$(mktemp -d)

# Policy A: sets mtu=1400 (conflict) and adds address 10.99.0.1/24 (non-conflicting).
cat > "$POLICY_DIR/policy-a.yaml" <<'EOF'
kind: policy
name: policy-a
factory: static
priority: 100
states:
  - type: ethernet
    name: veth-test0
    mtu: 1400
    addresses:
      - 10.99.0.1/24
EOF

# Policy B: sets mtu=9000 (conflict with A at same priority 100).
cat > "$POLICY_DIR/policy-b.yaml" <<'EOF'
kind: policy
name: policy-b
factory: static
priority: 100
states:
  - type: ethernet
    name: veth-test0
    mtu: 9000
EOF

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" apply "$POLICY_DIR" 2>&1) || EXIT_CODE=$?

rm -rf "$POLICY_DIR"

# Conflict must produce exit code 1.
if [[ $EXIT_CODE -ne 1 ]]; then
    echo "FAIL: 301-conflict-warning: expected exit code 1 (conflict), got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# Output must contain a conflict warning.
if ! echo "$OUTPUT" | grep -qi "conflict"; then
    echo "FAIL: 301-conflict-warning: output does not mention 'conflict'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# Conflicting mtu field must NOT have been applied; veth-test0 must still have mtu 1500.
LINK_OUTPUT=$(ip link show veth-test0)
if ! echo "$LINK_OUTPUT" | grep -q "mtu 1500"; then
    echo "FAIL: 301-conflict-warning: conflicting mtu was applied (veth-test0 does not have mtu 1500)" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

# Non-conflicting address from policy-a must have been applied.
assert_has_address veth-test0 "10.99.0.1/24"

echo "PASS: 301-conflict-warning"
