#!/usr/bin/env python3
"""Tests for the package/verify chain; no build, no network, no real binary.

The point of these tests is the property that made the previous contract
impossible to implement: `manifest.json` and `SHA256SUMS` must never have to
contain each other's hash. They assert the acyclic ordering, and that every
tampering attempt a consumer must refuse is actually refused.
"""
from __future__ import annotations

import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import package_release as packaging  # noqa: E402
import verify_package as verifying  # noqa: E402


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Fixture(unittest.TestCase):
    """Builds a throwaway repository-look-alike plus one staged package."""

    target = "aarch64-apple-darwin"

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="ol-host-package-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        (self.repo / "crates/protocol/src").mkdir(parents=True)
        (self.repo / "crates/manifest/src").mkdir(parents=True)
        (self.repo / "crates/plugin-abi/src").mkdir(parents=True)
        (self.repo / "docs").mkdir()
        (self.repo / "examples/hello").mkdir(parents=True)
        (self.repo / "Cargo.toml").write_text(
            '[workspace]\n[workspace.package]\nversion = "0.1.0"\n', encoding="utf-8")
        (self.repo / "version.txt").write_text("0.1.0\n", encoding="utf-8")
        (self.repo / "crates/protocol/src/lib.rs").write_text(
            'pub const PROTOCOL_VERSION: &str = "1";\n', encoding="utf-8")
        (self.repo / "crates/manifest/src/lib.rs").write_text(
            'pub const API_VERSION: &str = "1";\n', encoding="utf-8")
        (self.repo / "crates/plugin-abi/src/lib.rs").write_text(
            "pub const ABI_VERSION: u32 = 1;\n", encoding="utf-8")
        (self.repo / "README.md").write_text("# readme\n", encoding="utf-8")
        (self.repo / "CHANGELOG.md").write_text("# changelog\n", encoding="utf-8")
        (self.repo / "docs/service-contract.md").write_text("# service\n", encoding="utf-8")
        (self.repo / "docs/release-contract.md").write_text("# release\n", encoding="utf-8")
        (self.repo / "examples/hello/extension.json").write_text("{}\n", encoding="utf-8")

        self.binary = self.root / "ol-host"
        self.binary.write_bytes(b"#!/bin/sh\nexit 0\n")
        self.binary.chmod(0o755)
        self.stage = self.root / f"oext-0.1.0-{self.target}"
        self.manifest = packaging.stage_package(
            self.repo, self.stage, target=self.target, channel="stable",
            version="0.1.0", host_version="0.1.0", binary=self.binary,
        )
        self.archive = packaging.create_archive(self.stage, self.root / "dist", self.target)
        packaging.write_sidecar(self.archive)

    def rewrite_manifest(self, **changes) -> None:
        manifest = json.loads((self.stage / "manifest.json").read_text(encoding="utf-8"))
        manifest.update(changes)
        (self.stage / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        # Keep the checksum chain internally consistent so a test can isolate
        # one rule instead of tripping over a stale SHA256SUMS.
        packaging.write_checksums(self.stage)
        if self.archive.exists():
            self.archive.unlink()
        if self.archive.with_name(self.archive.name + ".sha256").exists():
            self.archive.with_name(self.archive.name + ".sha256").unlink()
        self.archive = packaging.create_archive(self.stage, self.root / "dist", self.target)
        packaging.write_sidecar(self.archive)


class AcyclicChecksums(Fixture):
    def test_manifest_does_not_hash_itself_or_the_sums_file(self) -> None:
        files = self.manifest["files"]
        self.assertNotIn("manifest.json", files)
        self.assertNotIn("SHA256SUMS", files)

    def test_sums_cover_the_manifest_but_not_themselves(self) -> None:
        lines = (self.stage / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
        paths = [line.split("  ", 1)[1] for line in lines]
        self.assertIn("manifest.json", paths)
        self.assertNotIn("SHA256SUMS", paths)

    def test_sums_cover_every_other_file(self) -> None:
        paths = {line.split("  ", 1)[1]
                 for line in (self.stage / "SHA256SUMS").read_text(encoding="utf-8").splitlines()}
        on_disk = {path.relative_to(self.stage).as_posix()
                   for path in self.stage.rglob("*") if path.is_file()}
        self.assertEqual(paths, on_disk - {"SHA256SUMS"})

    def test_generation_is_repeatable(self) -> None:
        before = (self.stage / "SHA256SUMS").read_text(encoding="utf-8")
        packaging.write_checksums(self.stage)
        self.assertEqual(before, (self.stage / "SHA256SUMS").read_text(encoding="utf-8"))


class Acceptance(Fixture):
    def test_fresh_package_verifies(self) -> None:
        manifest = verifying.verify_archive(self.archive)
        self.assertEqual(manifest["entry"], "ol-host")
        self.assertIsNone(manifest["sandbox"])

    def test_stage_directory_verifies_without_archive(self) -> None:
        verifying.verify_tree(self.stage)

    def test_manifest_declares_the_binaries_it_ships(self) -> None:
        self.assertIn("bin/ol-host", self.manifest["files"])
        self.assertEqual(self.manifest["protocolVersion"], "1")
        self.assertEqual(self.manifest["abiVersion"], 1)


class Rejections(Fixture):
    def test_tampered_payload_fails(self) -> None:
        target = self.stage / "examples/hello/extension.json"
        target.write_text('{"tampered":true}\n', encoding="utf-8")
        with self.assertRaises(verifying.VerificationError):
            verifying.verify_tree(self.stage)

    def test_tampered_manifest_fails(self) -> None:
        manifest = json.loads((self.stage / "manifest.json").read_text(encoding="utf-8"))
        manifest["version"] = "9.9.9"
        (self.stage / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaises(verifying.VerificationError):
            verifying.verify_tree(self.stage)

    def test_extra_file_outside_the_checksums_fails(self) -> None:
        (self.stage / "docs/EXTRA.md").write_text("smuggled\n", encoding="utf-8")
        with self.assertRaises(verifying.VerificationError):
            verifying.verify_tree(self.stage)

    def test_declared_sandbox_must_be_present(self) -> None:
        self.rewrite_manifest(sandbox="oext-sandbox")
        with self.assertRaisesRegex(verifying.VerificationError, "sandbox"):
            verifying.verify_tree(self.stage)

    def test_declared_entry_must_be_present(self) -> None:
        self.rewrite_manifest(entry="oext-host")
        with self.assertRaises(verifying.VerificationError):
            verifying.verify_tree(self.stage)

    def test_runtime_state_in_package_fails(self) -> None:
        (self.stage / "run").mkdir()
        (self.stage / "run/host.json").write_text("{}\n", encoding="utf-8")
        packaging.write_checksums(self.stage)
        archive = packaging.create_archive(self.stage, self.root / "dist", self.target)
        packaging.write_sidecar(archive)
        with self.assertRaisesRegex(verifying.VerificationError, "runtime state"):
            verifying.verify_archive(archive)

    def test_corrupt_archive_fails_sidecar_check(self) -> None:
        self.archive.write_bytes(b"corrupted")
        with self.assertRaisesRegex(verifying.VerificationError, "archive sha256"):
            verifying.verify_archive(self.archive)

    def test_expected_sha256_from_out_of_band_must_match(self) -> None:
        with self.assertRaisesRegex(verifying.VerificationError, "out-of-band"):
            verifying.verify_archive(self.archive, expected_sha256="0" * 64)


class Versions(unittest.TestCase):
    def test_stable_version_uses_the_workspace_version(self) -> None:
        self.assertEqual(packaging.package_version("0.1.0", "stable"), "0.1.0")

    def test_stable_version_must_match_the_release_tag(self) -> None:
        with self.assertRaises(SystemExit):
            packaging.package_version("0.1.0", "stable", release_tag="v0.2.0")

    def test_nightly_version_carries_run_and_commit(self) -> None:
        commit = "0123456789abcdef0123456789abcdef01234567"
        version = packaging.package_version("0.1.0", "nightly", commit=commit, run_id="42")
        self.assertEqual(version, "0.1.0-nightly.42.01234567")

    def test_nightly_requires_commit_and_run_id(self) -> None:
        with self.assertRaises(SystemExit):
            packaging.package_version("0.1.0", "nightly", commit="", run_id="42")

    def test_workspace_version_mirrors_version_txt(self) -> None:
        root = Path(__file__).resolve().parents[2]
        self.assertEqual(packaging.workspace_version(root),
                         (root / "version.txt").read_text(encoding="utf-8").strip())

    def test_versions_are_read_from_the_code_not_duplicated(self) -> None:
        versions = packaging.versions(Path(__file__).resolve().parents[2])
        self.assertEqual(versions["protocolVersion"], "1")
        self.assertEqual(versions["abiVersion"], 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
