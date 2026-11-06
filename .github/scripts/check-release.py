"""Validate that the workspace crates, version.txt and (when present) the release tag agree.

No longer depends on Release Please: the single source of truth is the workspace
Cargo.toml version. version.txt must mirror it, and every workspace member crate
(and its Cargo.lock entry) must match. When run against a release tag, the tag
must equal ``v<version>``.
"""
import os
import tomllib
from pathlib import Path

workspace = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["workspace"]
version = workspace["package"]["version"]

assert Path("version.txt").read_text().strip() == version, "version.txt drift"

names = set()
for member in workspace["members"]:
    package = tomllib.loads(Path(member, "Cargo.toml").read_text())["package"]
    assert package["version"] == version, f"{member}: version drift"
    names.add(package["name"])

lock = tomllib.loads(Path("Cargo.lock").read_text())
for package in lock["package"]:
    if package["name"] in names and "source" not in package:
        assert package["version"] == version, f"Cargo.lock: {package['name']} drift"

ref = os.environ.get("RELEASE_REF", "")
if ref.startswith("refs/tags/"):
    ref = ref.removeprefix("refs/tags/")
elif ref.startswith("refs/"):
    ref = ""
if ref:
    assert ref == f"v{version}", f"Tag {ref} does not match v{version}"

print(f"Workspace and {len(names)} crates agree on {version}")
