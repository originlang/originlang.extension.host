"""Exercise nightly publication with a fake gh executable; no network writes."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("publish-nightly.sh").resolve()
SHA = "0123456789abcdef0123456789abcdef01234567"


class NightlyTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="oext-nightly-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "dist").mkdir()
        (self.root / "bin").mkdir()
        mock = self.root / "bin/gh"
        mock.write_text("""#!/usr/bin/env bash
set -eu
printf '%s\n' "$*" >> ../calls.log
if [[ "$1 $2" == 'release view' ]]; then
  [[ "$MOCK_RELEASE" != 'missing' ]] || exit 1
  case "$*" in
    *isDraft*) [[ "$MOCK_RELEASE" == 'draft' ]] && echo true || echo false ;;
    *targetCommitish*) echo "$MOCK_SHA" ;;
    *) echo '{}' ;;
  esac
elif [[ "$1" == 'api' ]]; then
  echo "$MOCK_SHA"
fi
""", newline="\n")
        mock.chmod(0o755)
        for suffix in ["windows.zip", "linux.tar.gz"]:
            asset = self.root / "dist" / f"oext-0.1.0-nightly.123456.{SHA[:8]}-{suffix}"
            asset.write_bytes(b"fixture")
            digest = hashlib.sha256(asset.read_bytes()).hexdigest()
            Path(str(asset) + ".sha256").write_text(f"{digest}  {asset.name}\n", newline="\n")
        self.env = dict(os.environ, GITHUB_REF="refs/heads/feature/dev",
                        GITHUB_EVENT_NAME="push", GITHUB_SHA=SHA,
                        GITHUB_RUN_ID="123456", GITHUB_SERVER_URL="https://github.com",
                        GH_REPO="test/repo", MOCK_RELEASE="missing", MOCK_SHA=SHA)
        self.env["PATH"] = str(self.root / "bin") + os.pathsep + os.environ["PATH"]

    def run_script(self, **changes):
        return subprocess.run([os.environ.get("BASH_BIN", "bash"), str(SCRIPT)],
                              cwd=self.root, env=dict(self.env, **changes),
                              capture_output=True, text=True, timeout=15)

    def calls(self):
        log = self.root / "calls.log"
        return log.read_text() if log.exists() else ""

    def test_first_release_is_prerelease_for_exact_commit(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertIn(f"--target {SHA}", calls)
        self.assertIn("--draft --prerelease --latest=false", calls)
        self.assertIn("release edit nightly-123456-01234567 --draft=false", calls)

    def test_retry_uploads_without_creating_release(self):
        for state in ["published", "draft"]:
            with self.subTest(state=state):
                (self.root / "calls.log").write_text("")
                result = self.run_script(MOCK_RELEASE=state)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("release upload", self.calls())
                self.assertNotIn("release create", self.calls())

    def test_existing_release_for_different_commit_is_rejected(self):
        result = self.run_script(MOCK_RELEASE="published", MOCK_SHA="f" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("release upload", self.calls())

    def test_pull_request_and_other_branch_never_publish(self):
        for change in [{"GITHUB_EVENT_NAME": "pull_request"},
                       {"GITHUB_REF": "refs/heads/main"}]:
            result = self.run_script(**change)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(self.calls(), "")

    def test_corrupt_archive_never_publishes(self):
        next((self.root / "dist").glob("*.zip")).write_bytes(b"corrupt")
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), "")


if __name__ == "__main__":
    unittest.main()
