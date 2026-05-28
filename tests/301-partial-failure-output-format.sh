#!/bin/bash
# 301-partial-failure-output-format.sh
# AC: "Partial failure reports mixed results"
#
# When one policy succeeds and one fails, the output must:
#   1. Show the successful change (mentioning veth-test0).
#   2. Show the failure (mentioning the nonexistent interface).
#   3. Exit with code 1 (partial failure).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-partial-failure-output-format: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_DIR=$(mktemp -d)

# Policy for the existing interface — change its MTU (should succeed).
cat > "$POLICY_DIR/veth-test0.yaml" <<'EOF'
selector:
  name: veth-test0
mtu: 1400
EOF

# Policy for a non-existent interface — should fail.
cat > "$POLICY_DIR/veth-nonexistent99.yaml" <<'EOF'
selector:
  name: veth-nonexistent99
mtu: 1400
EOF

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" apply "$POLICY_DIR" 2>&1) || EXIT_CODE=$?

rm -rf "$POLICY_DIR"

# AC: partial failure must produce exit code 1.
if [[ $EXIT_CODE -ne 1 ]]; then
    echo "FAIL: 301-partial-failure-output-format: expected exit code 1, got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: output must show the successful change for veth-test0.
if ! echo "$OUTPUT" | grep -q "veth-test0"; then
    echo "FAIL: 301-partial-failure-output-format: output does not mention successful entity 'veth-test0'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

# AC: output must show the failure for the nonexistent interface.
if ! echo "$OUTPUT" | grep -q "veth-nonexistent99"; then
    echo "FAIL: 301-partial-failure-output-format: output does not mention failed entity 'veth-nonexistent99'" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-partial-failure-output-format"
