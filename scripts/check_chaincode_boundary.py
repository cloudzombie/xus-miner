#!/usr/bin/env python3
"""Fail CI if the standalone miner regains a writable/local chaincode coupling."""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def fail(message: str) -> None:
    print(f"chaincode boundary violation: {message}", file=sys.stderr)
    raise SystemExit(1)


manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
manifest_data = tomllib.loads(manifest)
if "../chain" in manifest or "../sov" in manifest:
    fail("Cargo.toml references a sibling SOV checkout")

dependency_tables = [manifest_data.get("dependencies", {})]
dependency_tables.extend(
    target.get("dependencies", {})
    for target in manifest_data.get("target", {}).values()
    if isinstance(target, dict)
)
for dependencies in dependency_tables:
    for name, specification in dependencies.items():
        if isinstance(specification, dict) and "path" in specification:
            fail(f"local path dependency is not allowed: {name}")

dependencies = manifest_data.get("dependencies", {})
for name, specification in dependencies.items():
    if isinstance(specification, str):
        version = specification
    elif isinstance(specification, dict):
        if "git" in specification:
            fail(f"Git dependency is not allowed: {name}")
        version = specification.get("version", "")
    else:
        fail(f"unrecognized dependency specification: {name}")
    if not re.fullmatch(r"=[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", version):
        fail(f"direct dependency must use an exact version: {name}")

lockfile = (ROOT / "Cargo.lock").read_text(encoding="utf-8")
if "git+" in lockfile or "cloudzombie/sov" in lockfile:
    fail("Cargo.lock contains a Git or SOV source dependency")

if (ROOT / ".gitmodules").exists():
    fail("Git submodules are not allowed")

for path in ROOT.rglob("*"):
    if ".git" in path.parts or "target" in path.parts:
        continue
    if path.is_symlink():
        fail(f"repository symlink is not allowed: {path.relative_to(ROOT)}")

for source in [ROOT / "src", ROOT / "tests", ROOT / "scripts"]:
    for path in source.rglob("*"):
        if not path.is_file() or path == Path(__file__):
            continue
        text = path.read_text(encoding="utf-8", errors="ignore")
        if "../chain" in text or "../sov" in text or "/Users/josh/github/sov" in text:
            fail(f"source references a local SOV checkout: {path.relative_to(ROOT)}")

workflow_dir = ROOT / ".github" / "workflows"
for workflow in workflow_dir.glob("*.yml"):
    text = workflow.read_text(encoding="utf-8")
    if not re.search(r"(?m)^permissions:\s*\n\s+contents:\s*read\s*$", text):
        fail(f"workflow lacks top-level contents: read permission: {workflow.name}")
    if re.search(r"(?m)^\s+[a-z-]+:\s*write\s*$", text):
        fail(f"workflow requests write permission: {workflow.name}")
    if "persist-credentials: false" not in text:
        fail(f"workflow persists checkout credentials: {workflow.name}")

print("chaincode boundary: clean")
