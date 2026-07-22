#!/usr/bin/env python3
"""Fail CI if the standalone miner regains a writable/local chaincode coupling."""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
DEPENDENCY_TABLES = {"dependencies", "dev-dependencies", "build-dependencies"}


def fail(message: str) -> None:
    print(f"chaincode boundary violation: {message}", file=sys.stderr)
    raise SystemExit(1)


manifest_path = ROOT / "Cargo.toml"
manifest = manifest_path.read_text(encoding="utf-8")
manifest_data = tomllib.loads(manifest)
if "../chain" in manifest or "../sov" in manifest:
    fail("Cargo.toml references a sibling SOV checkout")

if "workspace" in manifest_data:
    fail("Cargo workspaces are not allowed in the standalone repository")
if "patch" in manifest_data or "replace" in manifest_data:
    fail("Cargo source overrides are not allowed")
if (ROOT / "build.rs").exists() or "build" in manifest_data.get("package", {}):
    fail("repository build scripts are not allowed")
for cargo_config in (ROOT / ".cargo" / "config", ROOT / ".cargo" / "config.toml"):
    if cargo_config.exists():
        fail(f"Cargo source/runner configuration is not allowed: {cargo_config.name}")

target_sections: list[tuple[str, object]] = []
if "lib" in manifest_data:
    target_sections.append(("lib", manifest_data["lib"]))
for section in ("bin", "test", "example", "bench"):
    for index, target in enumerate(manifest_data.get(section, [])):
        target_sections.append((f"{section}[{index}]", target))
for name, target in target_sections:
    if not isinstance(target, dict):
        fail(f"invalid Cargo target table: {name}")
    raw_path = target.get("path")
    if raw_path is None:
        continue
    target_path = (ROOT / raw_path).resolve()
    if not target_path.is_relative_to(ROOT) or not target_path.is_file():
        fail(f"Cargo target escapes or is missing from repository: {name}.path")


def dependency_tables(value: object, location: str = "Cargo.toml"):
    """Yield every direct, dev, build, and target-specific dependency table."""
    if not isinstance(value, dict):
        return
    for name, child in value.items():
        child_location = f"{location}.{name}"
        if name in DEPENDENCY_TABLES:
            if not isinstance(child, dict):
                fail(f"invalid dependency table: {child_location}")
            yield child_location, child
        elif isinstance(child, dict):
            yield from dependency_tables(child, child_location)


for table_name, dependencies in dependency_tables(manifest_data):
    for name, specification in dependencies.items():
        if isinstance(specification, str):
            version = specification
        elif isinstance(specification, dict):
            forbidden = {"path", "git", "registry", "workspace"}.intersection(specification)
            if forbidden:
                fields = ", ".join(sorted(forbidden))
                fail(f"forbidden dependency source ({fields}): {table_name}.{name}")
            version = specification.get("version", "")
        else:
            fail(f"unrecognized dependency specification: {table_name}.{name}")
        if not re.fullmatch(r"=[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", version):
            fail(f"direct dependency must use an exact version: {table_name}.{name}")

lockfile_path = ROOT / "Cargo.lock"
lockfile = lockfile_path.read_text(encoding="utf-8")
if "git+" in lockfile or "cloudzombie/sov" in lockfile:
    fail("Cargo.lock contains a Git or SOV source dependency")
lock_data = tomllib.loads(lockfile)
for package in lock_data.get("package", []):
    source = package.get("source")
    if source is None:
        if package.get("name") != manifest_data["package"]["name"]:
            fail(f"unexpected local package in Cargo.lock: {package.get('name', '<unknown>')}")
        continue
    if source != CRATES_IO:
        fail(f"non-crates.io package source in Cargo.lock: {package.get('name', '<unknown>')}")
    if not re.fullmatch(r"[0-9a-f]{64}", package.get("checksum", "")):
        fail(f"missing/invalid crate checksum: {package.get('name', '<unknown>')}")

if (ROOT / ".gitmodules").exists():
    fail("Git submodules are not allowed")

for path in ROOT.rglob("*"):
    if ".git" in path.parts or "target" in path.parts:
        continue
    if path.is_symlink():
        fail(f"repository symlink is not allowed: {path.relative_to(ROOT)}")

for source in [ROOT / "src", ROOT / "tests", ROOT / "scripts"]:
    for path in source.rglob("*"):
        if not path.is_file() or path == Path(__file__).resolve():
            continue
        text = path.read_text(encoding="utf-8", errors="ignore")
        if "../chain" in text or "../sov" in text or "/Users/josh/github/sov" in text:
            fail(f"source references a local SOV checkout: {path.relative_to(ROOT)}")
        if path.suffix == ".rs" and re.search(
            r"\b(?:include|include_str|include_bytes)!\s*\(", text
        ):
            fail(f"compile-time file inclusion is not allowed: {path.relative_to(ROOT)}")

ruby = shutil.which("ruby")
if ruby is None:
    fail("Ruby with its standard Psych YAML parser is required for workflow validation")
workflow_check = subprocess.run(
    [ruby, str(ROOT / "scripts" / "check_workflows.rb")], check=False
)
if workflow_check.returncode != 0:
    raise SystemExit(workflow_check.returncode)

print("chaincode boundary: clean")
