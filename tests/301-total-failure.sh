#!/bin/bash
# 301-total-failure.sh
# AC: "Total failure returns exit code 2"
#
# When the only policy targets a non-existent interface, every operation fails.
# Exit code must be 2.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-total-failure: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

# Force daemon-free mode.
export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

# Do NOT create any veth pair — veth-nonexistent99 must be absent.

POLICY_FILE=$(mktemp --suffix=.yaml)
cat > "$POLICY_FILE" <<'EOF'
type: ethernet
name: veth-nonexistent99
mtu: 1400
EOF

EXIT_CODE=0
OUTPUT=$("$NETFYR_BIN" apply "$POLICY_FILE" 2>&1) || EXIT_CODE=$?

rm -f "$POLICY_FILE"

# Total failure must produce exit code 2.
if [[ $EXIT_CODE -ne 2 ]]; then
    echo "FAIL: 301-total-failure: expected exit code 2 (total failure), got $EXIT_CODE" >&2
    echo "      output: $OUTPUT" >&2
    exit 1
fi

echo "PASS: 301-total-failure"
