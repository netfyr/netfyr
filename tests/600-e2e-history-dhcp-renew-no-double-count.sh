#!/bin/bash
# 600-e2e-history-dhcp-renew-no-double-count.sh
# End-to-end: When a DHCP renewal produces a journal entry where the
# current addresses are plain strings and the desired addresses are
# objects (with lifetime metadata), "netfyr history" must NOT show the
# same address as both added (+) and removed (-).
#
# Also verifies that when the total number of address changes is between
# 3 and 8, each address is shown individually instead of being
# abbreviated as "(-N addrs)".
#
# Reproduces two CLI formatting bugs:
#   1. changes_summary() compares address list items by raw JSON
#      equality, so "10.0.0.1/24" != {"address":"10.0.0.1/24",...}.
#   2. format_address_changes() abbreviates removed addresses even
#      when the full values would fit in the column.
#
# Requires: unshare, ip (iproute2), jq
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

NETFYR_BIN="${NETFYR_BIN:-$SCRIPT_DIR/../target/debug/netfyr}"

if [[ ! -x "$NETFYR_BIN" ]]; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: netfyr binary not found at $NETFYR_BIN" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: jq not found" >&2
    exit 1
fi

netns_setup "$@"

# ---------- Inside the namespace ----------

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

JOURNAL_DIR="$TMPDIR_TEST/journal"
mkdir -p "$JOURNAL_DIR/archive"

NOW=$(date -u +%Y-%m-%dT%H:%M:%S.000000000Z)

# ── Bug 1: same address in string vs object form ──────────────────────────

# Craft a journal entry that mimics a DHCP renewal where the current
# state has the address as a plain string and the desired state has it
# as an object with lifetime metadata.  The CLI must recognise these as
# the same address and report no change.
cat > "$JOURNAL_DIR/current.ndjson" <<EOF
{"seq":1,"timestamp":"$NOW","trigger":{"type":"dhcp_event","policy_name":"dhcp-test","event_kind":"lease_renewed"},"active_policies":[{"name":"dhcp-test","factory_type":"dhcpv4","priority":100}],"diff":{"operations":[{"kind":"modify","entity_type":"ethernet","entity_name":"veth0","field_changes":[{"field_name":"addresses","change_kind":"set","current":["172.25.0.101/24"],"desired":[{"address":"172.25.0.101/24","preferred_lft":3600,"valid_lft":7200}]}]}]},"state_after":{"entities":[{"entity_type":"ethernet","selector_name":"veth0","fields":{"addresses":[{"address":"172.25.0.101/24","preferred_lft":3600,"valid_lft":7200}]}}]},"outcome":{"kind":"applied","succeeded":1,"failed":0,"skipped":0}}
EOF

HISTORY_OUTPUT=$(NO_COLOR=1 \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 10 2>&1) || true

RENEW_LINE=$(echo "$HISTORY_OUTPUT" | grep "dhcp-renew" | head -n 1)

if [[ -z "$RENEW_LINE" ]]; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: no dhcp-renew entry in history output" >&2
    echo "      output:" >&2
    echo "$HISTORY_OUTPUT" >&2
    exit 1
fi

# The same address must NOT appear as both added and removed.
if echo "$RENEW_LINE" | grep -qF "+172.25.0.101/24" &&
   echo "$RENEW_LINE" | grep -qF -- "-172.25.0.101/24"; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: address 172.25.0.101/24 shown as both added and removed" >&2
    echo "      line: $RENEW_LINE" >&2
    exit 1
fi

# Since the address did not actually change, CHANGES should show "(no changes)".
if ! echo "$RENEW_LINE" | grep -qF "(no changes)"; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: unchanged address should produce '(no changes)' in CHANGES" >&2
    echo "      line: $RENEW_LINE" >&2
    exit 1
fi

# ── Bug 2: unnecessary abbreviation with 3-8 address changes ─────────────

# Craft a second journal entry with 1 added + 2 removed addresses
# (total 3).  All three addresses must be shown individually; the CLI
# must not abbreviate the second removal as "(-1 addrs)".
cat > "$JOURNAL_DIR/current.ndjson" <<EOF
{"seq":2,"timestamp":"$NOW","trigger":{"type":"dhcp_event","policy_name":"dhcp-test","event_kind":"lease_renewed"},"active_policies":[{"name":"dhcp-test","factory_type":"dhcpv4","priority":100}],"diff":{"operations":[{"kind":"modify","entity_type":"ethernet","entity_name":"veth0","field_changes":[{"field_name":"addresses","change_kind":"set","current":["10.0.0.1/24","10.0.0.2/24"],"desired":["10.0.0.3/24"]}]}]},"state_after":{"entities":[{"entity_type":"ethernet","selector_name":"veth0","fields":{"addresses":["10.0.0.3/24"]}}]},"outcome":{"kind":"applied","succeeded":1,"failed":0,"skipped":0}}
EOF

HISTORY_OUTPUT2=$(NO_COLOR=1 \
    NETFYR_JOURNAL_DIR="$JOURNAL_DIR" \
    "$NETFYR_BIN" history -n 10 2>&1) || true

RENEW_LINE2=$(echo "$HISTORY_OUTPUT2" | grep "dhcp-renew" | head -n 1)

if [[ -z "$RENEW_LINE2" ]]; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: no dhcp-renew entry in second history output" >&2
    echo "      output:" >&2
    echo "$HISTORY_OUTPUT2" >&2
    exit 1
fi

# All three addresses must appear individually (no abbreviation).
if echo "$RENEW_LINE2" | grep -qE '\(-?[0-9]+ addrs?\)'; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: 3 address changes should not be abbreviated" >&2
    echo "      line: $RENEW_LINE2" >&2
    exit 1
fi

if ! echo "$RENEW_LINE2" | grep -qF "+10.0.0.3/24"; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: added address +10.0.0.3/24 not shown" >&2
    echo "      line: $RENEW_LINE2" >&2
    exit 1
fi

if ! echo "$RENEW_LINE2" | grep -qF -- "-10.0.0.1/24"; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: removed address -10.0.0.1/24 not shown" >&2
    echo "      line: $RENEW_LINE2" >&2
    exit 1
fi

if ! echo "$RENEW_LINE2" | grep -qF -- "-10.0.0.2/24"; then
    echo "FAIL: 600-e2e-history-dhcp-renew-no-double-count: removed address -10.0.0.2/24 not shown" >&2
    echo "      line: $RENEW_LINE2" >&2
    exit 1
fi

echo "PASS: 600-e2e-history-dhcp-renew-no-double-count"
