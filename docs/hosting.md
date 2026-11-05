# API documentation

The site at `https://netfyr.github.io/netfyr/` contains rustdoc output for the workspace's
public library APIs. Its home page lists libraries from Cargo metadata, so new
library crates appear when they join the workspace. Internal binaries and
dependency references are not built. There is no generated user manual or YAML
reference in this checkout. User guides live at [docs.netfyr.io](https://docs.netfyr.io/),
published from [netfyr/docs](https://github.com/netfyr/docs).

## Build locally

Install Python 3.9 or newer and stable Rust, then run:

```sh
RUSTUP_TOOLCHAIN=stable make docs
python3 -m http.server 8000 --bind 127.0.0.1 --directory target/pages
```

Open `http://localhost:8000`. `make docs` runs
`cargo doc --locked --workspace --lib --no-deps` with rustdoc warnings treated as
errors. It cleans the dedicated documentation output first, builds the landing
page, and checks local links and referenced assets before replacing `target/pages`.
Compilation output is kept in `target/docs-build` for subsequent builds.

## Enable GitHub Pages

The `API documentation` workflow checks pull requests and builds `main`.
Only `main` can deploy, including manual workflow runs. The deployment uses the
`github-pages` environment and uploads only `target/pages`.

After this change is merged into `netfyr/netfyr`:

1. In the repository's **Settings → Pages**, select **GitHub Actions** as the
   source. Leave the custom domain empty; `docs.netfyr.io` belongs to `netfyr/docs`.
2. Run the **API documentation** workflow on `main` if the initial deployment
   needs retrying after Pages is enabled.
3. Check `https://netfyr.github.io/netfyr/` and the library links.

The API site uses GitHub's default project URL and needs no DNS change.

See GitHub's [custom workflow guide](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages).
