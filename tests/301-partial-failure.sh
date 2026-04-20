#!/bin/bash
# 301-partial-failure.sh
# AC: "Partial failure reports mixed results"
#
# When a policy targets both an existing interface (veth-test0) and a
# non-existent one (veth-nonexistent99), the existing interface change
# should succeed and the non-existent one should fail. Exit code must be 1.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-partial-failure: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_DIR=$(mktemp -d)

# Policy for the existing interface — change its MTU.
cat > "$POLICY_DIR/veth-test0.yaml" <<'EOF'
type: ethernet
name: veth-test0
mtu: 1400
EOF

# Policy for a non-existent interface — this should fail at apply time.
cat > "$POLICY_DIR/veth-nonexistent99.yaml" <<'EOF'
type: ethernet
name: veth-nonexistent99
mtu: 1400
EOF

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" apply "$POLICY_DIR" 2>&1) || EXIT_CODE=$?

rm -rf "$POLICY_DIR"

# Partial failure must produce exit code 1.
if [[ $EXIT_CODE -ne 1 ]]; then
    echo "FAIL: 301-partial-failure: expected exit code 1 (partial failure), got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# The successful change for veth-test0 must be reflected in the kernel.
LINK_OUTPUT=$(ip link show veth-test0)
if ! echo "$LINK_OUTPUT" | grep -q "mtu 1400"; then
    echo "FAIL: 301-partial-failure: veth-test0 does not have mtu 1400 after partial apply" >&2
    echo "      ip link output: $LINK_OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-partial-failure"
