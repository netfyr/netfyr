#!/bin/bash
# 412-dhcpv6-stateless.sh
# Integration test: DHCPv6 stateless mode (O flag) wired through ipv6auto factory.
# Delegates to 411-dhcpv6-stateless-acquire.sh which covers the same scenario.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=helpers.sh
source "$SCRIPT_DIR/helpers.sh"

exec "$SCRIPT_DIR/411-dhcpv6-stateless-acquire.sh" "$@"
