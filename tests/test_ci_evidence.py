import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("ci_evidence", Path(__file__).resolve().parents[1] / "scripts/ci-evidence.py")
evidence = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evidence)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.output = Path(self.directory.name) / "evidence.json"

    def record(self, *args):
        with patch.object(sys, "argv", ["ci-evidence", "--output", str(self.output), *args]):
            evidence.main()
        return json.loads(self.output.read_text())

    def test_cancelled_work_stays_incomplete(self):
        record = self.record("--check", "rust=success", "--check", "shell=cancelled")
        self.assertEqual(record["outcome"], "incomplete")
        self.assertEqual(record["checks"]["shell"], "cancelled")
        self.assertRegex(record["source"]["revision"], r"^(?:[a-f0-9]{40}|[a-f0-9]{64})$")
        self.assertIsNone(record["taskManifest"])

    def test_missing_unknown_duplicate_and_invalid_revision_inputs_fail(self):
        for args in [(), ("--check", "rust=passed"), ("--check", "rust=success", "--check", "rust=failure"),
                     ("--check", "rust=success", "--requirements-revision", "a" * 41)]:
            with self.subTest(args=args), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                self.record(*args)
            self.assertFalse(self.output.exists())

    def test_preexisting_evidence_is_not_overwritten(self):
        self.output.write_text('{"outcome":"passed","stale":true}')
        with self.assertRaises(FileExistsError):
            self.record("--check", "rust=failure")
        self.assertTrue(json.loads(self.output.read_text())["stale"])

    def test_retains_manifest_bytes_without_claiming_source_authority(self):
        manifest = {"schemaVersion": 1, "kind": "specdoc-task-context", "sources": [], "declaredGaps": ["Unresolved dependency"]}
        manifest["id"] = hashlib.sha256(json.dumps(manifest, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()
        path = Path(self.directory.name) / "task.json"
        raw = json.dumps(manifest, indent=2) + "\n"
        path.write_text(raw)
        record = self.record("--check", "rust=success", "--task-manifest", str(path))
        self.assertEqual(record["taskManifest"]["raw"], raw)
        self.assertEqual(record["taskManifest"]["sha256"], hashlib.sha256(raw.encode()).hexdigest())
        self.assertTrue(record["taskManifest"]["identityVerified"])
        self.assertFalse(record["taskManifest"]["sourcesVerified"])

    def test_arbitrary_or_tampered_metadata_cannot_claim_manifest_identity(self):
        path = Path(self.directory.name) / "task.json"
        for value in [{"schemaVersion": 1}, {"schemaVersion": 1, "kind": "specdoc-task-context", "id": "0" * 64}]:
            path.write_text(json.dumps(value))
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                self.record("--check", "rust=success", "--task-manifest", str(path))
            self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
