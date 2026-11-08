#!/usr/bin/env python3
"""Record candidate identity and actual CI step outcomes without running checks."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def command(*args):
    return subprocess.run(args, check=True, text=True, capture_output=True).stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--check", action="append", default=[], metavar="NAME=OUTCOME")
    parser.add_argument("--requirements-revision", default="")
    parser.add_argument("--task-manifest", type=Path)
    args = parser.parse_args()
    checks = {}
    for item in args.check:
        name, separator, outcome = item.partition("=")
        if not separator or not re.fullmatch(r"[a-z][a-z0-9_-]*", name) or name in checks:
            parser.error("each check needs a unique NAME=OUTCOME")
        if outcome not in {"success", "failure", "skipped", "cancelled"}:
            parser.error(f"unknown outcome for {name}")
        checks[name] = outcome
    if not checks:
        parser.error("at least one actual check outcome is required")
    if args.requirements_revision and not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", args.requirements_revision):
        parser.error("requirements revision must be a full Git object ID")
    record = {
        "schemaVersion": 1,
        "kind": "development-checks",
        "source": {
            "repository": os.environ.get("GITHUB_REPOSITORY"),
            "revision": command("git", "rev-parse", "HEAD"),
            "dirty": bool(command("git", "-c", "core.fsmonitor=false", "status", "--porcelain", "--untracked-files=no")),
        },
        "requirements": {"repository": "netfyr/specs", "revision": args.requirements_revision or None},
        "checks": checks,
        "outcome": "failed" if "failure" in checks.values() else "passed" if all(value == "success" for value in checks.values()) else "incomplete",
        "tools": {},
        "limits": ["Step success records execution evidence, not full feature or requirement conformance."],
    }
    for name in ("rustc", "cargo"):
        try:
            record["tools"][name] = command(name, "--version")
        except (FileNotFoundError, subprocess.CalledProcessError):
            record["tools"][name] = None
    if args.task_manifest:
        raw = args.task_manifest.read_bytes()
        manifest = json.loads(raw)
        if not isinstance(manifest, dict) or manifest.get("schemaVersion") != 1 or manifest.get("kind") != "specdoc-task-context":
            parser.error("unsupported task manifest")
        content = {key: value for key, value in manifest.items() if key != "id"}
        digest = hashlib.sha256(json.dumps(content, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode("utf-8")).hexdigest()
        if manifest.get("id") != digest:
            parser.error("task manifest content identity does not match its id")
        record["taskManifest"] = {"sha256": hashlib.sha256(raw).hexdigest(), "id": digest,
                                  "raw": raw.decode("utf-8"), "identityVerified": True, "sourcesVerified": False}
        record["limits"].append("The recorder retains manifest bytes and checks their content identity; it does not verify source Git objects or merged authority.")
    else:
        record["taskManifest"] = None
        record["limits"].append("No pinned task-context manifest was supplied.")
    with args.output.open("x", encoding="utf-8") as output:
        json.dump(record, output, indent=2)
        output.write("\n")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Cannot record CI evidence: {error}", file=sys.stderr)
        sys.exit(1)
