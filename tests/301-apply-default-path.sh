#!/bin/bash
# 301-apply-default-path.sh
# AC: "netfyr apply with no path arguments defaults to /etc/netfyr/policies/"
#
# When no path arguments are given, netfyr apply reads from the default
# policy directory. The default can be overridden via NETFYR_APPLY_DEFAULT_DIR.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 301-apply-default-path: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi

export NETFYR_SOCKET_PATH=/nonexistent

netns_setup "$@"

# ---------- Inside the namespace ----------

create_veth veth-test0 veth-test1

# -- Test 1: default path with a valid policy directory --

POLICY_DIR=$(mktemp -d)
cat > "$POLICY_DIR/eth0.yaml" <<'EOF'
type: ethernet
name: veth-test0
mtu: 1400
EOF

EXIT_CODE=0
NETFYR_APPLY_DEFAULT_DIR="$POLICY_DIR" "$NETFYR_BIN" apply || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 301-apply-default-path: default dir apply expected exit 0, got $EXIT_CODE" >&2
    exit 1
fi

assert_mtu veth-test0 1400

# -- Test 2: explicit path overrides default --

EXPLICIT_FILE=$(mktemp --suffix=.yaml)
cat > "$EXPLICIT_FILE" <<'EOF'
type: ethernet
name: veth-test0
mtu: 1300
EOF

EXIT_CODE=0
NETFYR_APPLY_DEFAULT_DIR="$POLICY_DIR" "$NETFYR_BIN" apply "$EXPLICIT_FILE" || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 0 ]]; then
    echo "FAIL: 301-apply-default-path: explicit path expected exit 0, got $EXIT_CODE" >&2
    exit 1
fi

assert_mtu veth-test0 1300

# -- Test 3: nonexistent default path produces error --

EXIT_CODE=0
NETFYR_APPLY_DEFAULT_DIR="/nonexistent/default/path" \
    "$NETFYR_BIN" apply 2>/dev/null || EXIT_CODE=$?

if [[ $EXIT_CODE -ne 2 ]]; then
    echo "FAIL: 301-apply-default-path: nonexistent default expected exit 2, got $EXIT_CODE" >&2
    exit 1
fi

echo "PASS: 301-apply-default-path"
