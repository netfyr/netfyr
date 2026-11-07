.PHONY: test fmt clippy hooks docs

# make exports a command-line TAGS into the recipe environment, which the runner
# reads. Interpolating it into the command line instead would let a tag containing
# a quote run as shell.
test:
	@scripts/run-tests.sh

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings

hooks:
	@scripts/setup-hooks.sh

docs:
	RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --lib --no-deps
