import copy
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts.homebrew_release import needs_update, render_formula
from scripts import homebrew_release

ROOT = Path(__file__).resolve().parent.parent
PLATFORMS = ("macos-arm64", "macos-x64", "linux-arm64", "linux-x64")


class HomebrewReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.version = "0.21.1"
        self.assets = []
        for platform in PLATFORMS:
            name = f"bobby-browser-{self.version}-{platform}.tar.gz"
            payload = f"archive for {platform}".encode()
            (self.root / name).write_bytes(payload)
            self.assets.append({
                "name": name, "state": "uploaded", "size": len(payload),
                "digest": "sha256:" + hashlib.sha256(payload).hexdigest(),
                "browser_download_url": f"https://github.com/cavi-ai/bobby-browser/releases/download/v{self.version}/{name}",
            })
        self.release = {
            "tag_name": "v" + self.version, "draft": False,
            "prerelease": False, "assets": self.assets,
        }

    def prepare(self, release=None, version=None, formula=None):
        metadata = self.root / "release.json"
        metadata.write_text(json.dumps(self.release if release is None else release))
        template = self.root / "template.rb"
        template.write_text(formula if formula is not None else (ROOT / "Formula/bobby-browser.rb").read_text())
        output = self.root / "candidate.rb"
        result = subprocess.run([
            sys.executable, str(ROOT / "scripts/homebrew_release.py"), "prepare",
            "--tag", "v" + (version or self.version), "--release", str(metadata),
            "--assets-dir", str(self.root), "--formula", str(template),
            "--output", str(output),
        ], capture_output=True, text=True)
        return result, output

    def test_prepares_every_verified_platform_and_preserves_install_recipe(self):
        result, output = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        candidate = output.read_text()
        self.assertIn('version "0.21.1"', candidate)
        for asset in self.assets:
            self.assertIn('sha256 "' + asset["digest"].removeprefix("sha256:") + '"', candidate)
        original = (ROOT / "Formula/bobby-browser.rb").read_text()
        self.assertEqual(candidate.split("  def install", 1)[1], original.split("  def install", 1)[1])

    def test_rejects_a_changed_archive_without_writing_a_formula(self):
        (self.root / self.assets[0]["name"]).write_bytes(b"changed")
        result, output = self.prepare()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("archive integrity", result.stderr)
        self.assertFalse(output.exists())

    def test_rejects_missing_duplicate_and_invalid_asset_metadata(self):
        for mutation in ("missing", "duplicate", "digest", "url"):
            with self.subTest(mutation=mutation):
                release = copy.deepcopy(self.release)
                if mutation == "missing":
                    release["assets"].pop()
                elif mutation == "duplicate":
                    release["assets"].append(release["assets"][0])
                elif mutation == "digest":
                    release["assets"][0]["digest"] = None
                else:
                    release["assets"][0]["browser_download_url"] = "https://other.test/archive"
                result, output = self.prepare(release)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("release asset", result.stderr)
                self.assertFalse(output.exists())

    def test_rejects_unpublished_and_mismatched_releases(self):
        for field, value in (("draft", True), ("prerelease", True), ("tag_name", "v0.21.0")):
            with self.subTest(field=field):
                release = copy.deepcopy(self.release)
                release[field] = value
                result, output = self.prepare(release)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("published stable release", result.stderr)
                self.assertFalse(output.exists())

    def test_rejects_invalid_versions_and_incomplete_formula_templates(self):
        for version in ("../main", "0.21.1-rc.1", "01.2.3"):
            result, _ = self.prepare(version=version)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("stable release tag", result.stderr)
        original = (ROOT / "Formula/bobby-browser.rb").read_text()
        result, output = self.prepare(formula=original.replace("-linux-x64.tar.gz", "-linux-other.tar.gz"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("formula platforms", result.stderr)
        self.assertFalse(output.exists())

    def test_publication_is_idempotent_and_refuses_recipe_drift_or_downgrades(self):
        template = (ROOT / "Formula/bobby-browser.rb").read_text()
        hashes = {platform: "a" * 64 for platform in PLATFORMS}
        old = render_formula(template, "0.21.0", hashes)
        candidate = render_formula(template, "0.21.1", hashes)
        newer = render_formula(template, "0.22.0", hashes)
        self.assertTrue(needs_update(old, candidate))
        self.assertFalse(needs_update(candidate, candidate))
        self.assertFalse(needs_update(newer, candidate))
        with self.assertRaisesRegex(ValueError, "already published"):
            needs_update(candidate.replace("a" * 64, "b" * 64), candidate)
        with self.assertRaisesRegex(ValueError, "recipe differs"):
            needs_update(old.replace('bin.install "bobby"', 'bin.install "other"'), candidate)

    def test_a_retry_on_a_new_base_cannot_reuse_the_previous_formula_head(self):
        self.assertTrue(callable(getattr(homebrew_release, "validate_commit", None)))
        previous_base, current_base = "a" * 40, "b" * 40
        record = {"commit": {"verification": {"verified": True}}, "parents": [{"sha": previous_base}]}
        homebrew_release.validate_commit(record, previous_base)
        with self.assertRaisesRegex(ValueError, "current default branch"):
            homebrew_release.validate_commit(record, current_base)
        self.assertNotEqual(
            homebrew_release.publication_branch(self.version, previous_base),
            homebrew_release.publication_branch(self.version, current_base),
        )
        record["commit"]["verification"]["verified"] = False
        with self.assertRaisesRegex(ValueError, "verified signature"):
            homebrew_release.validate_commit(record, previous_base)


if __name__ == "__main__":
    unittest.main()
