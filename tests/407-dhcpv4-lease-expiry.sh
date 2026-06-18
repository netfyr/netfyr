#!/bin/bash
# 407-dhcpv4-lease-expiry.sh -- DHCPv4 lease expiry and re-acquisition (SPEC-407).
#
# Delegates to 600-e2e-dhcp-lease-expiry.sh which covers the full 12-step
# scenario from the spec.
#
# NOTE: This test takes approximately 2-3 minutes because the minimum dnsmasq
#       lease time is 120 seconds.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

exec "$SCRIPT_DIR/600-e2e-dhcp-lease-expiry.sh" "$@"
