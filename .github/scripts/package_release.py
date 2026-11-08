#!/usr/bin/env python3
"""Stage, checksum and archive one `ol-host` release package.

Package layout (docs/release-contract.md §3):

    oext-<version>-<target>/
    ├── bin/ol-host(.exe)
    ├── manifest.json
    ├── SHA256SUMS
    ├── docs/{README,CHANGELOG,service-contract,release-contract}.md
    └── examples/...

Checksum ordering is deliberately acyclic. `manifest.json` hashes every payload
file (everything except `manifest.json` and `SHA256SUMS`); `SHA256SUMS` then
hashes every file including `manifest.json`, but not itself; the archive hash is
published next to the archive as `<archive>.sha256`, which is the only value a
consumer has to obtain out of band. So:

    archive.sha256  →  archive  →  SHA256SUMS (incl. manifest.json)
                   →  manifest.files  →  payload files

No file has to contain its own hash, so the chain can always be produced and
always be verified.

The script is dependency-free (stdlib only) and runs on Linux, macOS and
Windows so stable and nightly builds share one code path.

Local packaging is the same code path, not a parallel one:

    cargo build --locked --release -p originlang-ext-host-bin
    python3 .github/scripts/package_release.py --build --verify

With no `--target` the host triple is read from `rustc -vV`, so the resulting
package is always labelled with the platform it was actually built for. A plain
`cargo build --release` (binary in `target/release/`) and a cross build
(binary in `target/<triple>/release/`) are both accepted.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tomllib
import zipfile
from pathlib import Path

SCHEMA_VERSION = 2
PACKAGE_NAME = "oext"
ENTRY_STEM = "ol-host"
# Cargo package that produces `bin/ol-host`; `--build` uses the same selection
# as CI so a local package cannot be built from a different binary target.
BIN_PACKAGE = "originlang-ext-host-bin"
# Metadata these files must never be hashed by `manifest.files`: `manifest.json`
# is hashed by SHA256SUMS instead, and SHA256SUMS cannot hash itself.
MANIFEST_FILE = "manifest.json"
SUMS_FILE = "SHA256SUMS"
REQUIRED_DOCS = (
    "service-contract.md",
    "release-contract.md",
)
# Files that must never appear in a release package: they are runtime state of
# an installation root, not deliverables.
FORBIDDEN_TOP_LEVEL = ("run", "state", "data", "logs")


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def workspace_version(root: Path) -> str:
    version = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"][
        "package"
    ]["version"]
    mirrored = (root / "version.txt").read_text(encoding="utf-8").strip()
    if mirrored != version:
        raise SystemExit(f"version.txt ({mirrored}) and Cargo.toml ({version}) disagree")
    return version


def rust_const(root: Path, relative: str, name: str, kind: str = "str") -> str:
    """Read a `pub const NAME: &str = "x"` / `...: u32 = 1` from Rust source.

    The manifest must state the versions the binary actually implements, so they
    are read from the same constants the code uses instead of being duplicated
    here. `smoke_host.py` cross-checks the result against the live handshake.
    """
    pattern = {
        "str": rf'pub const {name}: &str = "([^"]+)";',
        "u32": rf"pub const {name}: u32 = (\d+);",
    }[kind]
    match = re.search(pattern, (root / relative).read_text(encoding="utf-8"))
    if not match:
        raise SystemExit(f"cannot find {name} in {relative}")
    return match.group(1)


def versions(root: Path) -> dict[str, object]:
    return {
        "protocolVersion": rust_const(root, "crates/protocol/src/lib.rs", "PROTOCOL_VERSION"),
        "apiVersion": rust_const(root, "crates/manifest/src/lib.rs", "API_VERSION"),
        "abiVersion": int(rust_const(root, "crates/plugin-abi/src/lib.rs", "ABI_VERSION", "u32")),
    }


def entry_name(target: str) -> str:
    return f"{ENTRY_STEM}.exe" if "windows" in target else ENTRY_STEM


def host_target(*, required: bool = True) -> str | None:
    """The triple of the machine doing the build, read from the compiler.

    Local packaging must work without spelling out the triple, and must not
    label a host binary as a cross build, so the value comes from the same
    toolchain that will produce the binary instead of a hardcoded table.
    """
    rustc = os.environ.get("RUSTC") or shutil.which("rustc")
    if not rustc:
        if not required:
            return None
        raise SystemExit(
            "cannot detect the host target: rustc is not on PATH "
            "(install the toolchain pinned in rust-toolchain.toml, or pass --target)"
        )
    try:
        probe = subprocess.run([rustc, "-vV"], capture_output=True, text=True, check=True)
    except (OSError, subprocess.CalledProcessError) as error:
        if not required:
            return None
        raise SystemExit(f"cannot run `{rustc} -vV`: {error}") from error
    match = re.search(r"^host:\s*(\S+)$", probe.stdout, re.MULTILINE)
    if not match:
        if not required:
            return None
        raise SystemExit(f"cannot read the host triple from `{rustc} -vV`")
    return match.group(1)


def build_binary(root: Path, target: str, *, explicit_target: bool) -> None:
    """Build the entry point with the workspace's own locked release profile."""
    cargo = os.environ.get("CARGO") or shutil.which("cargo")
    if not cargo:
        raise SystemExit("--build needs cargo on PATH (or CARGO set)")
    command = [cargo, "build", "--locked", "--release", "-p", BIN_PACKAGE]
    if explicit_target:
        command.extend(["--target", target])
    print("build     " + " ".join(command))
    completed = subprocess.run(command, cwd=root)
    if completed.returncode != 0:
        raise SystemExit(f"cargo build failed with exit code {completed.returncode}")


def resolve_binary(root: Path, target: str, *, explicit: str | None,
                   host: str | None) -> Path:
    """Find the built entry point.

    Accepts `target/<triple>/release/` (cross or explicit `--target`) and
    `target/release/` (plain `cargo build --release` on the host). A host
    binary is never accepted for a different triple: that would mislabel the
    package.
    """
    if explicit:
        given = Path(explicit)
        binary = given if given.is_absolute() or given.is_file() else root / given
        if not binary.is_file():
            raise SystemExit(f"missing binary: {binary}")
        return binary
    candidates = [root / "target" / target / "release" / entry_name(target)]
    if host is not None and target == host:
        candidates.append(root / "target" / "release" / entry_name(target))
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    tried = ", ".join(str(path.relative_to(root)) for path in candidates)
    raise SystemExit(
        f"missing binary ({tried}); build it first, e.g. "
        f"`cargo build --locked --release -p {BIN_PACKAGE}`, or pass --build"
    )


def package_version(version: str, channel: str, *, release_tag: str = "",
                    commit: str = "", run_id: str = "") -> str:
    if channel == "stable":
        if release_tag:
            tag_version = release_tag[1:] if release_tag.startswith("v") else release_tag
            if tag_version != version:
                raise SystemExit(
                    f"RELEASE_TAG ({tag_version}) and workspace version ({version}) disagree"
                )
        return version
    if not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise SystemExit("nightly packaging requires a 40-character commit SHA")
    if not re.fullmatch(r"\d+", run_id):
        raise SystemExit("nightly packaging requires a numeric run id")
    return f"{version}-nightly.{run_id}.{commit[:8]}"


def copy_docs(root: Path, stage: Path) -> None:
    docs = stage / "docs"
    docs.mkdir(parents=True, exist_ok=True)
    for source, name in (("README.md", "README.md"), ("CHANGELOG.md", "CHANGELOG.md")):
        shutil.copyfile(root / source, docs / name)
    for name in REQUIRED_DOCS:
        shutil.copyfile(root / "docs" / name, docs / name)


def copy_examples(root: Path, stage: Path) -> None:
    source = root / "examples"
    if source.is_dir():
        shutil.copytree(source, stage / "examples", dirs_exist_ok=True)


def payload_files(stage: Path) -> dict[str, str]:
    """`path → sha256` for every file except manifest.json and SHA256SUMS."""
    files: dict[str, str] = {}
    for path in sorted(stage.rglob("*")):
        if not path.is_file():
            continue
        relative = path.relative_to(stage).as_posix()
        if relative in (MANIFEST_FILE, SUMS_FILE):
            continue
        files[relative] = sha256_file(path)
    return files


def build_manifest(root: Path, stage: Path, *, version: str, host_version: str,
                   channel: str, target: str, sandbox: str | None) -> dict:
    manifest = {
        "schemaVersion": SCHEMA_VERSION,
        "name": PACKAGE_NAME,
        "version": version,
        "hostVersion": host_version,
        "channel": channel,
        "target": target,
        "entry": entry_name(target),
        "sandbox": sandbox,
    }
    manifest.update(versions(root))
    manifest["files"] = payload_files(stage)
    return manifest


def write_manifest(stage: Path, manifest: dict) -> None:
    (stage / MANIFEST_FILE).write_text(
        json.dumps(manifest, indent=2, sort_keys=False) + "\n", encoding="utf-8", newline="\n"
    )


def write_checksums(stage: Path) -> None:
    """Hash every file in the package except SHA256SUMS itself.

    `manifest.json` is included here - that is what lets a consumer test the
    manifest against the package instead of trusting it as read.
    """
    lines = []
    for path in sorted(stage.rglob("*")):
        if not path.is_file():
            continue
        relative = path.relative_to(stage).as_posix()
        if relative == SUMS_FILE:
            continue
        lines.append(f"{sha256_file(path)}  {relative}\n")
    (stage / SUMS_FILE).write_text("".join(lines), encoding="utf-8", newline="\n")


def stage_package(root: Path, stage: Path, *, target: str, channel: str, version: str,
                  host_version: str, binary: Path, build_info: dict | None = None,
                  sandbox_binary: Path | None = None) -> dict:
    """Stage one package. `sandbox_binary` is only passed when a sandbox is
    actually shipped; today's release does not ship one, so the manifest says
    `sandbox: null` and the host reports `sandbox: null` at runtime."""
    if stage.exists():
        shutil.rmtree(stage)
    (stage / "bin").mkdir(parents=True)
    entry = entry_name(target)
    shutil.copyfile(binary, stage / "bin" / entry)
    if os.name != "nt":
        (stage / "bin" / entry).chmod(0o755)
    sandbox = None
    if sandbox_binary is not None:
        sandbox = "oext-sandbox.exe" if "windows" in target else "oext-sandbox"
        shutil.copyfile(sandbox_binary, stage / "bin" / sandbox)
        if os.name != "nt":
            (stage / "bin" / sandbox).chmod(0o755)
    copy_docs(root, stage)
    copy_examples(root, stage)
    if build_info is not None:
        (stage / "build-info.json").write_text(
            json.dumps(build_info, indent=2) + "\n", encoding="utf-8", newline="\n"
        )
    manifest = build_manifest(
        root, stage, version=version, host_version=host_version,
        channel=channel, target=target, sandbox=sandbox,
    )
    write_manifest(stage, manifest)
    write_checksums(stage)
    return manifest


def create_archive(stage: Path, destination: Path, target: str) -> Path:
    destination.mkdir(parents=True, exist_ok=True)
    root_name = stage.name
    if "windows" in target:
        archive = destination / f"{root_name}.zip"
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
            for path in sorted(stage.rglob("*")):
                if path.is_file():
                    bundle.write(path, f"{root_name}/{path.relative_to(stage).as_posix()}")
        return archive
    archive = destination / f"{root_name}.tar.gz"
    with tarfile.open(archive, "w:gz") as bundle:
        for path in sorted(stage.rglob("*")):
            if not path.is_file():
                continue
            relative = path.relative_to(stage).as_posix()
            info = bundle.gettarinfo(str(path), f"{root_name}/{relative}")
            info.uid, info.gid, info.uname, info.gname = 0, 0, "", ""
            info.mode = 0o755 if relative.startswith("bin/") else 0o644
            with path.open("rb") as handle:
                bundle.addfile(info, handle)
    return archive


def write_sidecar(archive: Path) -> Path:
    digest = sha256_file(archive)
    sidecar = archive.with_name(archive.name + ".sha256")
    sidecar.write_text(f"{digest}  {archive.name}\n", encoding="utf-8", newline="\n")
    return sidecar


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        epilog="With no arguments this packages the host platform: "
               "python3 .github/scripts/package_release.py --build --verify",
    )
    parser.add_argument(
        "--target",
        help="Rust target triple (default: detected from rustc, so a plain "
             "`cargo build --release` is packaged as the host platform)",
    )
    parser.add_argument("--channel", choices=("stable", "nightly"), default="stable")
    parser.add_argument("--dist", default="dist", help="output directory (default: dist)")
    parser.add_argument("--stage-root", default="target/package", help="staging parent")
    parser.add_argument("--binary", help="path to the built ol-host binary")
    parser.add_argument(
        "--build", action="store_true",
        help=f"run `cargo build --locked --release -p {BIN_PACKAGE}` before packaging",
    )
    parser.add_argument(
        "--verify", action="store_true",
        help="verify the archive and smoke-accept it (same scripts CI runs)",
    )
    parser.add_argument(
        "--sandbox-binary",
        help="path to a built oext-sandbox; omit it to ship a package with sandbox: null",
    )
    parser.add_argument("--release-tag", default=os.environ.get("RELEASE_TAG", ""))
    parser.add_argument("--commit", default=os.environ.get("GITHUB_SHA", ""))
    parser.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", ""))
    parser.add_argument(
        "--build-info-extra", action="append", default=[], metavar="KEY=VALUE",
        help="extra nightly build-info.json fields (e.g. branch=feature/dev)",
    )
    args = parser.parse_args(argv)

    root = repo_root()

    def from_root(path: str) -> Path:
        """Relative paths are resolved against the repository, not the shell's
        cwd, so packaging behaves the same from any directory."""
        candidate = Path(path)
        return candidate if candidate.is_absolute() else root / candidate

    explicit_target = args.target is not None
    if explicit_target:
        target = args.target
        host = host_target(required=False)
    else:
        target = host = host_target()

    host_version = workspace_version(root)
    version = package_version(
        host_version, args.channel, release_tag=args.release_tag,
        commit=args.commit, run_id=args.run_id,
    )

    if args.build:
        build_binary(root, target, explicit_target=explicit_target)
    binary = resolve_binary(root, target, explicit=args.binary, host=host)

    stage = from_root(args.stage_root) / f"{PACKAGE_NAME}-{version}-{target}"
    build_info = None
    if args.channel == "nightly":
        build_info = {
            "channel": args.channel,
            "version": host_version,
            "packageVersion": version,
            "commit": args.commit,
            "runId": args.run_id,
            "target": target,
        }
        for pair in args.build_info_extra:
            key, _, value = pair.partition("=")
            if not key:
                raise SystemExit(f"malformed --build-info-extra {pair!r}")
            build_info[key] = value
    manifest = stage_package(
        root, stage, target=target, channel=args.channel, version=version,
        host_version=host_version, binary=binary, build_info=build_info,
        sandbox_binary=from_root(args.sandbox_binary) if args.sandbox_binary else None,
    )
    archive = create_archive(stage, from_root(args.dist), target)
    sidecar = write_sidecar(archive)

    # Flush before handing over to the verify scripts, which write directly to
    # the inherited stdout: the package summary must not appear below them.
    print(
        "\n".join((
            f"target    {target}",
            f"package   {archive}",
            f"entry     bin/{manifest['entry']}",
            f"files     {len(manifest['files'])}",
            f"sha256    {sidecar.read_text(encoding='utf-8').split()[0]}",
        )),
        flush=True,
    )

    if args.verify:
        scripts = Path(__file__).resolve().parent
        for script in ("verify_package.py", "smoke_host.py"):
            completed = subprocess.run([sys.executable, str(scripts / script), str(archive)])
            if completed.returncode != 0:
                print(f"FAIL {script} rejected {archive.name}", file=sys.stderr)
                return completed.returncode
    return 0


if __name__ == "__main__":
    sys.exit(main())
