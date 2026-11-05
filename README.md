# netfyr

Declarative network configuration for Linux, applied over netlink.

You describe the network state you want in YAML. netfyr reads the current state from
the kernel, works out the difference, and applies it over netlink. The query and apply
paths need no daemon.

YAML values are decoded using the embedded schema at each property path. For
example, an address-looking interface name remains a string, while an
`ipv4.addresses[].ip` value uses the declared CIDR format.

Work here is driven spec by spec from
[netfyr/specs](https://github.com/netfyr/specs).

## Build and test

```
cargo build                    # compile the workspace
cargo test                     # unit tests
make test                      # shell integration tests
make test TAGS=ipv4,routing    # integration tests tagged ipv4 OR routing
make test TAGS=ipv4+routing    # integration tests tagged ipv4 AND routing
```

The workspace's first crate is `netfyr-state`, a library, so `cargo test` runs its
unit tests now; the shell suite under `make test` will exercise the CLI once a later
story lands one.

## API documentation

Run `make docs` to generate the public Rust API reference in `target/pages`.
The GitHub Pages workflow publishes `main` at `https://netfyr.github.io/netfyr/`.
See [documentation hosting](docs/hosting.md) for local preview and initial setup.

## License

MIT. See [LICENSE](LICENSE).
