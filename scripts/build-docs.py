#!/usr/bin/env python3
"""Build the workspace's public library reference for GitHub Pages."""

import html
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parent.parent
BUILD = ROOT / "target" / "docs-build"
OUTPUT = ROOT / "target" / "pages"


def cargo(*arguments, capture=False):
    environment = os.environ.copy()
    environment.pop("CARGO_ENCODED_RUSTDOCFLAGS", None)
    environment["RUSTDOCFLAGS"] = "-D warnings"
    return subprocess.run(
        ["cargo", *arguments],
        cwd=ROOT,
        env=environment,
        text=True,
        check=True,
        stdout=subprocess.PIPE if capture else None,
    ).stdout


class References(HTMLParser):
    def __init__(self):
        super().__init__()
        self.urls = []

    def handle_starttag(self, tag, attributes):
        for name, value in attributes:
            if name in {"href", "src"} and value:
                self.urls.append(value)


def check_links(site):
    site = site.resolve()
    count = 0
    for page in site.rglob("*.html"):
        parser = References()
        parser.feed(page.read_text(encoding="utf-8"))
        for reference in parser.urls:
            url = urlsplit(reference)
            if url.scheme or url.netloc or not url.path:
                continue
            if url.path.startswith("/"):
                raise ValueError(f"Root-relative link in {page}: {reference}")
            target = (page.parent / unquote(url.path)).resolve()
            if not target.is_relative_to(site) or not target.exists():
                raise ValueError(f"Broken local link in {page}: {reference}")
            if target.is_dir() and not (target / "index.html").is_file():
                raise ValueError(f"Missing directory index in {page}: {reference}")
            count += 1
    return count


def landing_page(libraries):
    entries = []
    for package, target in libraries:
        name = html.escape(package["name"])
        version = html.escape(package["version"])
        description = html.escape(package.get("description") or "")
        href = html.escape(target["name"], quote=True)
        entries.append(
            f'<li><h2><a href="{href}/index.html">{name}</a></h2>'
            f'<p>{description}</p><p class="version">{version}</p></li>'
        )
    return """<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>netfyr API documentation</title>
<style>
  :root { color-scheme: light dark; font-family: system-ui, sans-serif; }
  body { max-width: 52rem; margin: 0 auto; padding: 3rem 1.25rem; line-height: 1.6; }
  h1 { font-size: clamp(1.8rem, 5vw, 2.5rem); line-height: 1.2; }
  h2 { margin-bottom: 0; font-size: 1.3rem; }
  ul { list-style: none; padding: 0; }
  li { border-top: 1px solid #8888; padding: 1rem 0; }
  li p { margin: .25rem 0; }
  .version { font-family: monospace; }
  footer { border-top: 1px solid #8888; padding-top: 1rem; }
  a { text-underline-offset: .2em; }
</style>
<main>
  <h1>netfyr API documentation</h1>
  <p>Public Rust APIs generated from this source tree.</p>
  <ul>
""" + "\n".join(entries) + """
  </ul>
</main>
<footer>
  <a href="https://github.com/netfyr/netfyr">Source</a> ·
  <a href="https://github.com/netfyr/specs">Specifications</a> ·
  <a href="https://design.netfyr.io">Design library</a>
</footer>
</html>
"""


def main():
    metadata = json.loads(cargo("metadata", "--locked", "--no-deps", "--format-version", "1", capture=True))
    members = set(metadata["workspace_members"])
    library_kinds = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
    libraries = sorted(
        (
            (package, target)
            for package in metadata["packages"]
            if package["id"] in members
            for target in package["targets"]
            if library_kinds.intersection(target["kind"])
        ),
        key=lambda item: item[0]["name"],
    )
    if not libraries:
        raise ValueError("The workspace has no library targets to document")
    cargo("clean", "--doc", "--target-dir", str(BUILD))
    cargo("doc", "--locked", "--workspace", "--lib", "--no-deps", "--target-dir", str(BUILD))
    with tempfile.TemporaryDirectory(prefix="pages-", dir=OUTPUT.parent) as temporary:
        site = Path(temporary) / "site"
        shutil.copytree(BUILD / "doc", site)
        for _, target in libraries:
            if not (site / target["name"] / "index.html").is_file():
                raise ValueError(f"Missing generated library reference: {target['name']}")
        (site / "index.html").write_text(landing_page(libraries), encoding="utf-8")
        (site / ".nojekyll").touch()
        links = check_links(site)
        if OUTPUT.exists():
            shutil.rmtree(OUTPUT)
        site.rename(OUTPUT)
    print(f"Built {len(libraries)} libraries; checked {links} local links in {OUTPUT}")


if __name__ == "__main__":
    main()
