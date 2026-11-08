# netfyr

Declarative network configuration for Linux, applied over netlink.

Configuration uses YAML. The workspace contains libraries for network state,
policy loading and validation, reconciliation, and the Linux network backend. The
end-user CLI is still in development. The intended query and apply paths need no
daemon.

YAML values are decoded using the embedded schema at each property path. For
example, an address-looking interface name remains a string, while an
`ipv4.addresses[].ip` value uses the declared CIDR format.

Work here is driven spec by spec from
[netfyr/specs](https://github.com/netfyr/specs). See the
[development guide](https://netfyr.io/development/) for the contribution workflow
and [CONTRIBUTING.md](CONTRIBUTING.md) for this repository's commands and rules.

## Build and test

```
cargo build                    # compile the workspace
cargo test                     # unit tests
make test                      # shell checks
make test TAGS=build,meta       # shell checks tagged build OR meta
make test TAGS=meta+policy      # shell checks tagged meta AND policy
```

`cargo test` runs the library tests. The shell suite checks the build and repository
conventions. Run privileged backend acceptance checks in a disposable VM, following
[the integration suite instructions](tests/integration/coverage.md).

## API documentation

Run `make docs` to generate the library API reference. Open
`target/doc/netfyr_state/index.html`; use the Crates sidebar to switch libraries.
User guides and configuration reference are at [docs.netfyr.io](https://docs.netfyr.io/).

## License

MIT. See [LICENSE](LICENSE).
