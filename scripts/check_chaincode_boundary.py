#!/usr/bin/env python3
"""Fail CI if the standalone miner regains a writable/local chaincode coupling."""

from __future__ import annotations

import re
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

workflow_dir = ROOT / ".github" / "workflows"
workflows = sorted((*workflow_dir.glob("*.yml"), *workflow_dir.glob("*.yaml")))
for workflow in workflows:
    text = workflow.read_text(encoding="utf-8")
    jobs_match = re.search(r"(?m)^jobs:\s*$", text)
    if jobs_match is None:
        fail(f"workflow has no jobs mapping: {workflow.name}")
    job_headers = list(
        re.finditer(
            r"(?m)^  (?:\"([^\"]+)\"|'([^']+)'|([A-Za-z0-9_-]+)):\s*$",
            text[jobs_match.end() :],
        )
    )
    job_ranges: list[tuple[str, int, int]] = []
    for index, header in enumerate(job_headers):
        name = next(group for group in header.groups() if group is not None)
        start = jobs_match.end() + header.start()
        end = (
            jobs_match.end() + job_headers[index + 1].start()
            if index + 1 < len(job_headers)
            else len(text)
        )
        job_ranges.append((name, start, end))

    # Parse permission mappings conservatively instead of searching for only the
    # spelling `contents: write`. YAML permits quoted keys, aliases, inline maps,
    # and duplicate mappings; accepting any of those here would make a textual
    # guard easy to bypass. Release workflows intentionally use one tiny,
    # canonical subset that this parser can prove unambiguous.
    lines = text.splitlines(keepends=True)
    offsets: list[int] = []
    offset = 0
    for line in lines:
        offsets.append(offset)
        offset += len(line)
    permission_blocks: list[tuple[int, int, dict[str, str]]] = []
    scalar_indent: int | None = None
    key_line = re.compile(
        r"^( *)(?:(\"[^\"]+\"|'[^']+'|[A-Za-z0-9_-]+)):\s*(.*?)\s*(?:#.*)?(?:\r?\n)?$"
    )
    for index, line in enumerate(lines):
        stripped = line.strip()
        indent = len(line) - len(line.lstrip(" "))
        if scalar_indent is not None:
            if not stripped or indent > scalar_indent:
                continue
            scalar_indent = None
        match = key_line.match(line)
        if match is None:
            continue
        raw_key = match.group(2)
        value = match.group(3).strip()
        if value in {"|", ">", "|-", ">-", "|+", ">+"}:
            scalar_indent = len(match.group(1))
            continue
        key = raw_key[1:-1] if raw_key[:1] in {"'", '"'} else raw_key
        if key != "permissions":
            continue
        if raw_key != "permissions":
            fail(f"quoted permissions key is not allowed: {workflow.name}")
        if value:
            fail(f"inline, aliased, or broad permissions are not allowed: {workflow.name}")

        entries: dict[str, str] = {}
        for child in lines[index + 1 :]:
            child_stripped = child.strip()
            child_indent = len(child) - len(child.lstrip(" "))
            if not child_stripped or child_stripped.startswith("#"):
                continue
            if child_indent <= indent:
                break
            child_match = key_line.match(child)
            if child_indent != indent + 2 or child_match is None:
                fail(f"ambiguous permissions mapping is not allowed: {workflow.name}")
            child_raw_key = child_match.group(2)
            child_value = child_match.group(3).strip()
            if child_raw_key[:1] in {"'", '"'}:
                fail(f"quoted permission key is not allowed: {workflow.name}")
            if child_value not in {"read", "write"}:
                fail(f"permission value must be literal read/write: {workflow.name}")
            if child_raw_key in entries:
                fail(f"duplicate permission key is not allowed: {workflow.name}")
            entries[child_raw_key] = child_value
        permission_blocks.append((indent, offsets[index], entries))

    top_level = [entries for indent, _, entries in permission_blocks if indent == 0]
    if top_level != [{"contents": "read"}]:
        fail(f"workflow must have exactly top-level contents: read: {workflow.name}")
    job_level = [block for block in permission_blocks if block[0] != 0]
    if workflow.name == "release.yml":
        if len(job_level) != 1:
            fail("release workflow must have exactly one job permission override")
        indent, position, entries = job_level[0]
        owners = [name for name, start, end in job_ranges if start <= position < end]
        expected = {
            "contents": "write",
            "id-token": "write",
            "attestations": "write",
        }
        if indent != 4 or owners != ["publish"] or entries != expected:
            fail("only jobs.publish may request the exact release write permissions")
    elif job_level:
        fail(f"non-release workflow requests job permissions: {workflow.name}")
    if "pull_request_target" in text:
        fail(f"pull_request_target is not allowed: {workflow.name}")
    if re.search(r"(?mi)runs-on:.*self-hosted", text):
        fail(f"self-hosted runners are not allowed: {workflow.name}")
    if "cloudzombie/sov" in text or "/github/sov" in text:
        fail(f"workflow references the SOV repository: {workflow.name}")

    remote_uses = re.findall(r"(?m)^\s*-?\s*uses:\s*([^\s#]+)", text)
    for action in remote_uses:
        if action.startswith("./"):
            continue
        if re.fullmatch(r"[^/@\s]+/[^/@\s]+@[0-9a-f]{40}", action) is None:
            fail(f"workflow action is not pinned to a full commit: {workflow.name}: {action}")

    checkout_blocks = re.findall(
        r"(?ms)^\s*-\s+uses:\s*actions/checkout@[0-9a-f]{40}.*?"
        r"(?=^\s*-\s+(?:uses|name):|\Z)",
        text,
    )
    if not checkout_blocks:
        fail(f"workflow has no pinned checkout step: {workflow.name}")
    for block in checkout_blocks:
        if not re.search(r"(?m)^\s+persist-credentials:\s*false\s*$", block):
            fail(f"checkout persists credentials: {workflow.name}")

print("chaincode boundary: clean")
