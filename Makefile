.PHONY: integration-test

# Run all integration test scripts in tests/[0-9]*.sh.
# Each script runs as a separate bash process inside its own network namespace.
# Tests run in parallel (controlled by JOBS, default: number of CPUs).
# Requires: bash, ip (iproute2), unshare (util-linux).
# Optional: dnsmasq (for DHCP tests).
integration-test:
	cargo build
	@scripts/run-integration-tests.sh $(SPEC)
