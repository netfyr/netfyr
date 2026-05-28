#!/bin/bash
# 103-apply-query-roundtrip.sh
# Integration test: Apply a policy then verify via netfyr query (JSON output).
# Mapped to spec shell scenario: "Full round-trip: apply then query".

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 103-apply-query-roundtrip: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
selector:
  name: veth-test0
mtu: 1400
addresses:
  - "10.99.0.1/24"
EOF

"$NETFYR_BIN" apply "$POLICY_FILE"
EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 103-apply-query-roundtrip: netfyr apply exited with code $EXIT_CODE" >&2
    exit 1
fi

QUERY_OUTPUT=$("$NETFYR_BIN" query \
    --selector type=ethernet \
    --selector name=veth-test0 \
    --output json)

if ! echo "$QUERY_OUTPUT" | grep -q '"mtu": 1400'; then
    echo "FAIL: 103-apply-query-roundtrip: query output does not show mtu=1400" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

if ! echo "$QUERY_OUTPUT" | grep -q '10.99.0.1/24'; then
    echo "FAIL: 103-apply-query-roundtrip: query output does not contain address 10.99.0.1/24" >&2
    echo "      query output: $QUERY_OUTPUT" >&2
    exit 1
fi

echo "PASS: 103-apply-query-roundtrip"
