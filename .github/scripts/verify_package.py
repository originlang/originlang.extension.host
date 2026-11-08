#!/usr/bin/env python3
"""Verify one `ol-host` release package before it is published or installed.

Verification chain (docs/release-contract.md §5), each level binding the next:

    1. archive sha256  = the value published beside the archive
                         (`<archive>.sha256`, or passed with --expected-sha256)
    2. SHA256SUMS      = every file in the package except itself, including
                         `manifest.json`
    3. manifest.files  = every payload file except `manifest.json` and
                         `SHA256SUMS`, cross-checked against SHA256SUMS
    4. entry/sandbox/兼容矩阵 = declared entry really exists, a declared sandbox
                         really exists, `sandbox: null` means no sandbox binary

Nothing has to hash itself, so the chain is always both producible and
verifiable. A consumer that only receives loose files can run steps 2-4 on an
extracted directory; the archive hash in step 1 is what it must obtain from the
publisher out of band (release page, download metadata, signature).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

ENTRY_STEM = "ol-host"
MANIFEST_FILE = "manifest.json"
SUMS_FILE = "SHA256SUMS"
SUPPORTED_SCHEMA = 2
REQUIRED_DOCS = ("docs/README.md", "docs/CHANGELOG.md",
                 "docs/service-contract.md", "docs/release-contract.md")
FORBIDDEN_TOP_LEVEL = ("run", "state", "data", "logs")
SUM_LINE = re.compile(r"^([a-f0-9]{64}) {2}(.+)$")


class VerificationError(Exception):
    """A package failed verification; the message names the failing rule."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_sums(text: str) -> dict[str, str]:
    sums: dict[str, str] = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        match = SUM_LINE.match(line)
        if not match:
            raise VerificationError(f"malformed SHA256SUMS line: {line!r}")
        digest, raw_path = match.groups()
        if raw_path.startswith("/") or ".." in Path(raw_path).parts:
            raise VerificationError(f"SHA256SUMS path escapes the package: {raw_path!r}")
        if raw_path in sums:
            raise VerificationError(f"SHA256SUMS lists {raw_path} twice")
        sums[raw_path] = digest
    if not sums:
        raise VerificationError("SHA256SUMS is empty")
    return sums


def entry_name(target: str) -> str:
    return f"{ENTRY_STEM}.exe" if "windows" in target else ENTRY_STEM


def expected_target_from_name(name: str) -> str | None:
    """`oext-<version>-<target>[.tar.gz|.zip]` → target triple (None when unrecognised)."""
    for suffix in (".tar.gz", ".zip"):
        if name.endswith(suffix):
            name = name[: -len(suffix)]
    for marker in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
                   "x86_64-apple-darwin", "aarch64-apple-darwin",
                   "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"):
        if name.endswith(marker):
            return marker
    return None


def verify_tree(root: Path, *, expected_target: str | None = None) -> dict:
    """Verifies an extracted package directory and returns its manifest."""
    manifest_path = root / MANIFEST_FILE
    sums_path = root / SUMS_FILE
    if not manifest_path.is_file():
        raise VerificationError("missing manifest.json at package root")
    if not sums_path.is_file():
        raise VerificationError("missing SHA256SUMS at package root")

    # Step 2: every file except SHA256SUMS itself, manifest.json included.
    sums = parse_sums(sums_path.read_text(encoding="utf-8"))
    actual = {
        path.relative_to(root).as_posix(): path
        for path in root.rglob("*")
        if path.is_file()
    }
    missing = set(actual) - set(sums)
    extra = set(sums) - set(actual)
    if extra:
        raise VerificationError(f"SHA256SUMS lists files that are not present: {sorted(extra)}")
    if missing - {SUMS_FILE}:
        raise VerificationError(
            f"files not covered by SHA256SUMS: {sorted(missing - {SUMS_FILE})}"
        )
    for relative, digest in sums.items():
        if sha256_file(actual[relative]) != digest:
            raise VerificationError(f"sha256 mismatch: {relative}")
    if MANIFEST_FILE not in sums:
        raise VerificationError("SHA256SUMS does not cover manifest.json, so the manifest is untrusted")
    if SUMS_FILE in sums:
        raise VerificationError("SHA256SUMS must not list itself")

    # Step 3: manifest fields, then its `files` map against SHA256SUMS.
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise VerificationError(f"manifest.json is not valid JSON: {error}") from error
    if not isinstance(manifest, dict):
        raise VerificationError("manifest.json must be a JSON object")
    schema = manifest.get("schemaVersion")
    if not isinstance(schema, int) or schema > SUPPORTED_SCHEMA:
        raise VerificationError(f"unsupported manifest schemaVersion: {schema!r}")
    if manifest.get("name") != "oext":
        raise VerificationError(f"unexpected package name: {manifest.get('name')!r}")
    for field, kind in (("version", str), ("hostVersion", str), ("channel", str),
                        ("target", str), ("entry", str), ("protocolVersion", str),
                        ("apiVersion", str), ("abiVersion", int), ("files", dict)):
        if not isinstance(manifest.get(field), kind):
            raise VerificationError(f"manifest field {field!r} is missing or has the wrong type")
    if manifest["channel"] not in ("stable", "nightly"):
        raise VerificationError(f"unknown channel: {manifest['channel']!r}")
    if not isinstance(manifest.get("sandbox"), (str, type(None))):
        raise VerificationError("manifest field 'sandbox' must be a string or null")
    if expected_target and manifest["target"] != expected_target:
        raise VerificationError(
            f"manifest target {manifest['target']} does not match package name ({expected_target})"
        )
    if manifest["entry"] != entry_name(manifest["target"]):
        raise VerificationError(
            f"entry {manifest['entry']!r} does not match target {manifest['target']}"
        )

    files = manifest["files"]
    for relative, digest in files.items():
        if relative in (MANIFEST_FILE, SUMS_FILE):
            raise VerificationError(
                f"manifest.files must not contain {relative}: that would make the checksum circular"
            )
        if relative not in sums:
            raise VerificationError(f"manifest.files lists {relative}, absent from SHA256SUMS")
        if sums[relative] != digest:
            raise VerificationError(f"manifest.files disagrees with SHA256SUMS for {relative}")

    # Step 4: the declared entry exists and is hashed; a sandbox is only
    # declared when the binary is really in the package.
    entry_path = f"bin/{manifest['entry']}"
    if entry_path not in files:
        raise VerificationError(f"declared entry {entry_path} is not covered by manifest.files")
    if not (root / entry_path).is_file():
        raise VerificationError(f"declared entry {entry_path} does not exist in the package")
    sandbox = manifest["sandbox"]
    if sandbox is not None:
        sandbox_path = f"bin/{sandbox}"
        if not (root / sandbox_path).is_file():
            raise VerificationError(f"manifest declares sandbox {sandbox_path}, but it is absent")
        if sandbox_path not in files:
            raise VerificationError(f"declared sandbox {sandbox_path} is not covered by manifest.files")
    for name in REQUIRED_DOCS:
        if name not in files:
            raise VerificationError(f"required document missing from package: {name}")
    payload = {path for path in actual if path not in (MANIFEST_FILE, SUMS_FILE)}
    if set(files) != payload:
        raise VerificationError(
            f"manifest.files does not cover the payload exactly; "
            f"missing={sorted(payload - set(files))} extra={sorted(set(files) - payload)}"
        )
    return manifest


def verify_archive(archive: Path, *, expected_sha256: str | None = None,
                   use_sidecar: bool = True) -> dict:
    """Verifies a `.tar.gz` / `.zip` package including its archive hash."""
    if not archive.is_file():
        raise VerificationError(f"no such archive: {archive}")
    digest = sha256_file(archive)
    sidecar = archive.with_name(archive.name + ".sha256")
    if use_sidecar and sidecar.is_file():
        line = sidecar.read_text(encoding="utf-8").strip()
        match = SUM_LINE.match(line)
        if not match or match.group(2) != archive.name:
            raise VerificationError(f"malformed checksum sidecar: {sidecar.name}")
        if match.group(1) != digest:
            raise VerificationError(f"archive sha256 does not match {sidecar.name}")
    elif use_sidecar:
        raise VerificationError(f"missing checksum sidecar: {sidecar.name}")
    if expected_sha256 and expected_sha256.lower() != digest:
        raise VerificationError(
            "archive sha256 does not match the out-of-band value: "
            f"expected {expected_sha256.lower()}, got {digest}"
        )

    with tempfile.TemporaryDirectory(prefix="ol-host-verify-") as temp:
        extract_to = Path(temp)
        if archive.name.endswith(".zip"):
            with zipfile.ZipFile(archive) as bundle:
                bundle.extractall(extract_to)
        else:
            with tarfile.open(archive) as bundle:
                try:
                    bundle.extractall(extract_to, filter="data")
                except TypeError:  # Python < 3.12 has no extraction filters
                    bundle.extractall(extract_to)
        roots = [path for path in extract_to.iterdir() if path.is_dir()]
        if len(roots) != 1:
            raise VerificationError(
                f"archive must contain exactly one top-level directory, found {len(roots)}"
            )
        root = roots[0]
        expected_target = expected_target_from_name(root.name)
        if expected_target is None:
            raise VerificationError(
                f"package directory name must end with a known target triple: {root.name}"
            )
        if expected_target_from_name(archive.name) != expected_target:
            raise VerificationError(
                f"archive name {archive.name} does not carry target {expected_target}"
            )
        for name in FORBIDDEN_TOP_LEVEL:
            if (root / name).exists():
                raise VerificationError(f"package contains runtime state directory: {name}/")
        if any(path.suffix == ".log" for path in root.rglob("*.log")):
            raise VerificationError("package contains a log file")
        manifest = verify_tree(root, expected_target=expected_target)
    return manifest


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("archive", nargs="?", help="package archive (.tar.gz or .zip)")
    parser.add_argument("--dir", help="verify an already extracted package directory instead")
    parser.add_argument("--expected-sha256", help="archive sha256 obtained out of band")
    parser.add_argument("--no-sidecar", action="store_true",
                        help="do not require <archive>.sha256 next to the archive")
    args = parser.parse_args(argv)
    if not args.archive and not args.dir:
        parser.error("pass an archive or --dir")

    try:
        if args.dir:
            manifest = verify_tree(Path(args.dir),
                                   expected_target=expected_target_from_name(Path(args.dir).name))
        else:
            manifest = verify_archive(Path(args.archive),
                                      expected_sha256=args.expected_sha256,
                                      use_sidecar=not args.no_sidecar)
    except VerificationError as error:
        print(f"FAIL {error}", file=sys.stderr)
        return 1
    print(f"OK   {manifest['name']} {manifest['version']} "
          f"({manifest['target']}, channel {manifest['channel']}, "
          f"entry {manifest['entry']}, sandbox {manifest['sandbox']}, "
          f"{len(manifest['files'])} files)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
