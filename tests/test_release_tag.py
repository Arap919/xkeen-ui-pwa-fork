import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/tag-upstream-release.yml"


class ReleaseTagTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "user.name", "Test")
        self.git("commit", "--allow-empty", "-qm", "initial")
        self.source = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/upstream-tags/v1.3.6", self.source)
        self.git("commit", "--allow-empty", "-qm", "sync merge")
        self.merge = self.git("rev-parse", "HEAD")

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, text=True).strip()

    def select(self):
        self.assertTrue(WORKFLOW.exists(), "Release tagging workflow is missing")
        lines = WORKFLOW.read_text().splitlines()
        marker = lines.index("      - name: Select release tag")
        start = lines.index("        run: |", marker) + 1
        end = next((i for i in range(start, len(lines)) if lines[i].startswith("      - ")), len(lines))
        script = textwrap.dedent("\n".join(lines[start:end]))
        output = self.repo / "output"
        output.write_text("")
        result = subprocess.run(["bash", "-euo", "pipefail", "-c", script], cwd=self.repo,
                                env={**os.environ, "SYNC_HEAD_SHA": self.source,
                                     "MERGE_SHA": self.merge, "GITHUB_OUTPUT": str(output)},
                                capture_output=True, text=True)
        return result, output.read_text()

    def test_same_version_increments_highest_numeric_suffix(self):
        self.git("tag", "v1.3.6+pwa.2", self.source)
        self.git("tag", "v1.3.6+pwa.10", self.source)
        result, output = self.select()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tag=v1.3.6+pwa.11", output)

    def test_new_upstream_version_resets_suffix(self):
        self.git("tag", "v1.3.6+pwa.10", self.source)
        self.git("update-ref", "refs/upstream-tags/v1.3.7", self.source)
        result, output = self.select()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tag=v1.3.7+pwa.1", output)

    def test_unmerged_and_prerelease_upstream_tags_are_ignored(self):
        self.git("update-ref", "refs/upstream-tags/v9.0.0", self.merge)
        self.git("update-ref", "refs/upstream-tags/v2.0.0-rc.1", self.source)
        result, output = self.select()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tag=v1.3.6+pwa.1", output)

    def test_rerun_skips_already_tagged_merge(self):
        self.git("tag", "v1.3.6+pwa.3", self.merge)
        result, output = self.select()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("exists=true", output)
        self.assertNotIn("tag=", output)

    def test_upstream_version_uses_numeric_order(self):
        self.git("update-ref", "refs/upstream-tags/v1.9.0", self.source)
        self.git("update-ref", "refs/upstream-tags/v1.10.0", self.source)
        result, output = self.select()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tag=v1.10.0+pwa.1", output)

    def test_annotated_upstream_tag_is_supported(self):
        self.git("tag", "-a", "v1.3.7", self.source, "-m", "Upstream release")
        self.git("update-ref", "refs/upstream-tags/v1.3.7", self.git("rev-parse", "v1.3.7"))
        result, output = self.select()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tag=v1.3.7+pwa.1", output)

    def test_no_stable_upstream_version_fails(self):
        self.git("update-ref", "-d", "refs/upstream-tags/v1.3.6")
        result, output = self.select()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No stable upstream tag", result.stderr)
        self.assertNotIn("tag=", output)


if __name__ == "__main__":
    unittest.main()
