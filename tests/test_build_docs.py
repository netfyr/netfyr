from pathlib import Path
import runpy
import tempfile
import unittest


check_links = runpy.run_path(str(Path(__file__).resolve().parents[1] / "scripts/build-docs.py"))["check_links"]


class DocumentationLinksTest(unittest.TestCase):
    def test_symlinked_output_accepts_internal_links(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            site = root / "actual"
            site.mkdir()
            (site / "index.html").write_text('<a href="library.html">Library</a>')
            (site / "library.html").write_text('<a href="index.html">Home</a>')
            (root / "target").symlink_to(site, target_is_directory=True)
            self.assertEqual(check_links(root / "target"), 2)

    def test_links_outside_the_site_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            site = root / "site"
            site.mkdir()
            (root / "private.html").write_text("Not published")
            (site / "index.html").write_text('<a href="../private.html">Outside</a>')
            with self.assertRaises(ValueError):
                check_links(site)


if __name__ == "__main__":
    unittest.main()
