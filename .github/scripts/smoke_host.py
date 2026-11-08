#!/usr/bin/env python3
"""Smoke-accept an extracted `ol-host` release package.

Runs the checks the delivery chain must satisfy before a package can be called
delivered (docs/release-contract.md §10, docs/service-contract.md §13):

  * `--version` prints exactly the manifest `hostVersion` on stdout, exit 0
  * `--help` exits 0, unknown arguments exit 2
  * a stdio session answers `host.hello` then `host.info`, then exits 0 when
    stdin reaches EOF (clean shutdown, nothing left in `run/`)
  * every stdout byte is a JSON-RPC frame: the startup banner and diagnostics
    stay on stderr
  * the handshake matches the manifest (protocol/api/abi) and honestly reports
    the available backends - no sandbox, so `sandbox` is null and `sandbox` is
    absent from `backends`
  * `--backend sandbox` fails on a package that ships no sandbox, instead of
    silently degrading to in-process execution
  * `host.shutdown` still stops the process with exit code 0

The script never needs a network, a CLI, or a sandbox binary; it drives only
what the package itself contains.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import zipfile
from pathlib import Path

from verify_package import VerificationError, verify_archive, verify_tree

FAILURES: list[str] = []


def check(condition: bool, message: str) -> None:
    if condition:
        print(f"  ok   {message}")
        return
    FAILURES.append(message)
    print(f"  FAIL {message}")


def extract(archive: Path, destination: Path) -> Path:
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as bundle:
            bundle.extractall(destination)
    else:
        with tarfile.open(archive) as bundle:
            try:
                bundle.extractall(destination, filter="data")
            except TypeError:
                bundle.extractall(destination)
    roots = [path for path in destination.iterdir() if path.is_dir()]
    if len(roots) != 1:
        raise VerificationError("archive must contain exactly one top-level directory")
    return roots[0]


def run(binary: Path, args: list[str], *, stdin_text: str | None = None,
        home: Path | None = None, timeout: float = 60.0) -> subprocess.CompletedProcess:
    command = [str(binary), *args]
    environment = dict(os.environ)
    # Isolate the run from any ambient installation root so the checks only see
    # state produced by this package.
    for name in ("OEXT_HOME", "OEXT_CONFIG", "OEXT_STATE_DIR", "OEXT_SANDBOX_BIN"):
        environment.pop(name, None)
    if home is not None:
        command.extend(["--home", str(home)])
    return subprocess.run(
        command,
        input=stdin_text,
        capture_output=True,
        text=True,
        timeout=timeout,
        env=environment,
    )


def assert_pure_frames(stdout: str, expected_ids: list[int]) -> dict[int, dict]:
    """Every stdout line must be one JSON-RPC response with an expected id."""
    frames: dict[int, dict] = {}
    for line in stdout.splitlines():
        if not line.strip():
            check(False, "stdout contains a blank line")
            continue
        try:
            frame = json.loads(line)
        except json.JSONDecodeError:
            check(False, f"stdout line is not JSON (log leak?): {line[:120]!r}")
            continue
        if frame.get("jsonrpc") != "2.0":
            check(False, f"stdout frame lacks jsonrpc 2.0: {line[:120]!r}")
            continue
        identifier = frame.get("id")
        if identifier not in expected_ids:
            check(False, f"stdout frame has unexpected id: {identifier!r}")
            continue
        if identifier in frames:
            check(False, f"stdout repeats response id {identifier!r}")
            continue
        frames[identifier] = frame
    return frames


def smoke(root: Path, manifest: dict) -> None:
    target = manifest["target"]
    entry = root / "bin" / manifest["entry"]
    check(entry.is_file(), f"entry exists: bin/{manifest['entry']}")
    if not entry.is_file():
        return
    if os.name != "nt":
        check(os.access(entry, os.X_OK), "entry is executable")

    workdir = Path(tempfile.mkdtemp(prefix="ol-host-smoke-"))
    home = workdir / "home"
    try:
        # --version: one line, no banner, exit 0 (§1).
        result = run(entry, ["--version"])
        check(result.returncode == 0, f"--version exits 0 (got {result.returncode})")
        check(result.stdout == f"{manifest['hostVersion']}\n",
              f"--version prints exactly {manifest['hostVersion']!r}")
        check(result.stderr == "", "--version writes nothing to stderr")

        result = run(entry, ["--help"])
        check(result.returncode == 0, f"--help exits 0 (got {result.returncode})")
        check("ol-host" in result.stdout, "--help names the ol-host entry")

        result = run(entry, ["--definitely-not-an-option"])
        check(result.returncode == 2, f"unknown argument exits 2 (got {result.returncode})")
        check(result.stdout == "", "unknown argument writes usage to stderr, not stdout")

        # stdio session: handshake, diagnostics, then EOF.
        frames_in = "".join(json.dumps(frame) + "\n" for frame in (
            {"jsonrpc": "2.0", "id": 1, "method": "host.hello",
             "params": {"protocolVersion": manifest["protocolVersion"],
                        "client": {"name": "smoke", "version": manifest["hostVersion"]}}},
            {"jsonrpc": "2.0", "id": 2, "method": "host.info"},
        ))
        started = time.monotonic()
        result = run(entry, ["--transport", "stdio"], stdin_text=frames_in, home=home)
        elapsed = time.monotonic() - started
        check(result.returncode == 0, f"stdio EOF exits 0 (got {result.returncode})")
        check(elapsed < 30, f"stdio EOF shutdown is prompt ({elapsed:.1f}s)")

        frames = assert_pure_frames(result.stdout, [1, 2])
        check(len(frames) == 2, f"exactly two responses on stdout (got {len(frames)})")
        check("ol-host" in result.stderr, "startup banner is on stderr")

        hello = frames.get(1, {}).get("result", {})
        info = frames.get(2, {}).get("result", {})
        check(hello.get("protocolVersion") == manifest["protocolVersion"],
              f"hello protocolVersion matches manifest ({manifest['protocolVersion']})")
        check(hello.get("apiVersion") == manifest["apiVersion"],
              f"hello apiVersion matches manifest ({manifest['apiVersion']})")
        check(hello.get("abiVersion") == manifest["abiVersion"],
              f"hello abiVersion matches manifest ({manifest['abiVersion']})")
        check(hello.get("hostVersion") == manifest["hostVersion"],
              f"hello hostVersion matches manifest ({manifest['hostVersion']})")
        check(str(hello.get("root", "")) == str(home),
              "hello reports the --home root")
        check(info.get("hostVersion") == hello.get("hostVersion"), "host.info mirrors host.hello")
        check("extensions" in info, "host.info reports extension counts")

        backends = hello.get("backends", [])
        check(isinstance(backends, list) and bool(backends), "hello reports available backends")
        if manifest["sandbox"] is None:
            check(hello.get("sandbox", "missing") is None,
                  "hello reports sandbox: null when the package ships none")
            check("sandbox" not in backends,
                  f"hello does not advertise the sandbox backend ({backends})")
        check(hello.get("runtime") in ("router", "quickjs", "dylib", "sandbox"),
              f"hello reports a known runtime ({hello.get('runtime')})")

        # Nothing from the session may be left behind (§9.2 step 5).
        run_dir = home / "run"
        leftovers = sorted(path.name for path in run_dir.iterdir()) if run_dir.is_dir() else []
        check(not leftovers, f"stdio shutdown leaves no run/ state (found {leftovers})")

        # A package without a sandbox must not pretend it has one.
        if manifest["sandbox"] is None:
            result = run(entry, ["--backend", "sandbox"], stdin_text="", home=home)
            check(result.returncode != 0,
                  "missing sandbox makes --backend sandbox fail instead of degrading")
            check("ol-host" in result.stderr, "the sandbox failure is explained on stderr")

        # Explicit shutdown still exits 0.
        frame = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "host.hello",
                            "params": {"protocolVersion": manifest["protocolVersion"]}})
        stop = json.dumps({"jsonrpc": "2.0", "id": 2, "method": "host.shutdown"})
        result = run(entry, ["--transport", "stdio"], stdin_text=f"{frame}\n{stop}\n", home=home)
        check(result.returncode == 0,
              f"host.shutdown exits 0 (got {result.returncode})")
        frames = assert_pure_frames(result.stdout, [1, 2])
        check(frames.get(2, {}).get("result", {}).get("stopping") is True,
              "host.shutdown answers {stopping: true}")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("archive", nargs="?", help="package archive (.tar.gz or .zip)")
    parser.add_argument("--dir", help="smoke an already extracted package directory")
    parser.add_argument("--expected-sha256", help="archive sha256 obtained out of band")
    args = parser.parse_args(argv)
    if not args.archive and not args.dir:
        parser.error("pass an archive or --dir")

    if args.dir:
        root = Path(args.dir)
        try:
            manifest = verify_tree(root)
        except VerificationError as error:
            print(f"FAIL verification: {error}", file=sys.stderr)
            return 1
        print(f"package {root.name} verified, smoking bin/{manifest['entry']}")
        smoke(root, manifest)
    else:
        archive = Path(args.archive)
        try:
            manifest = verify_archive(archive, expected_sha256=args.expected_sha256)
        except VerificationError as error:
            print(f"FAIL verification: {error}", file=sys.stderr)
            return 1
        print(f"package {archive.name} verified, smoking bin/{manifest['entry']}")
        with tempfile.TemporaryDirectory(prefix="ol-host-extract-") as temp:
            smoke(extract(archive, Path(temp)), manifest)

    if FAILURES:
        print(f"\nFAILED {len(FAILURES)} check(s):", file=sys.stderr)
        for failure in FAILURES:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("\nSmoke acceptance passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
