"""Validate the single workspace release version and Release Please coverage."""
import json
import os
import tomllib
from pathlib import Path

workspace = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["workspace"]
version = workspace["package"]["version"]
manifest = json.loads(Path(".release-please-manifest.json").read_text())
config = json.loads(Path("release-please-config.json").read_text())
expected_paths = {".", *workspace["members"]}
assert set(manifest) == expected_paths, "Release manifest must cover every member"
assert set(config["packages"]) == expected_paths, "Release config must cover every member"
assert Path("version.txt").read_text().strip() == version, "version.txt drift"
assert all(v == version for v in manifest.values()), "Release manifest version drift"
components = {"oext"}
names = set()
for member in workspace["members"]:
    package = tomllib.loads(Path(member, "Cargo.toml").read_text())["package"]
    assert package["version"] == version, f"{member}: version drift"
    names.add(package["name"])
    components.add(package["name"])
    assert config["packages"][member]["release-type"] == "rust"
linked = next(p for p in config["plugins"] if p["type"] == "linked-versions")
assert set(linked["components"]) == components, "Linked release group is incomplete"
assert any(p["type"] == "cargo-workspace" for p in config["plugins"])
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
print(f"Release configuration and {len(names)} crates agree on {version}")
