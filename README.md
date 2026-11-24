# netfyr

Declarative network configuration for Linux, applied over netlink.

You describe the network state you want in YAML. netfyr reads the current state from
the kernel, works out the difference, and applies it over netlink. The query and apply
paths need no daemon.

YAML values are decoded using the embedded schema at each property path. For
example, an address-looking interface name remains a string, while an
`ipv4.addresses[].ip` value uses the declared CIDR format.

Core features follow the merged requirements in [netfyr/specs](https://github.com/netfyr/specs).
See [Contributing to netfyr](CONTRIBUTING.md) for the development and review workflow.

## Build and test

```
cargo build                    # compile the workspace
cargo test                     # Rust workspace tests
make test                      # build and run host shell checks
```

The workspace contains the state, policy, reconciliation and backend libraries.
For live-kernel checks, follow the [backend acceptance guide](tests/integration/coverage.md)
to run the probe and pytest suite in a disposable VM.

## API documentation

Run `make docs` to generate the library API reference. Open
`target/doc/netfyr_state/index.html`; use the Crates sidebar to switch libraries.
User guides and configuration reference are at [docs.netfyr.io](https://docs.netfyr.io/).

## License

MIT. See [LICENSE](LICENSE).
