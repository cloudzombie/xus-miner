#!/usr/bin/env python3
"""Enforce one version across Cargo, the application, tags, and releases."""

from __future__ import annotations

import argparse
import os
import re
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CORE_NUMBER = r"(?:0|[1-9][0-9]*)"
PRERELEASE_IDENTIFIER = r"(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
BUILD_IDENTIFIER = r"[0-9A-Za-z-]+"
SEMVER = re.compile(
    rf"{CORE_NUMBER}\.{CORE_NUMBER}\.{CORE_NUMBER}"
    rf"(?:-{PRERELEASE_IDENTIFIER}(?:\.{PRERELEASE_IDENTIFIER})*)?"
    rf"(?:\+{BUILD_IDENTIFIER}(?:\.{BUILD_IDENTIFIER})*)?"
)


def fail(message: str) -> None:
    print(f"version contract violation: {message}", file=sys.stderr)
    raise SystemExit(1)


parser = argparse.ArgumentParser()
parser.add_argument("--tag", help="release tag to validate")
args = parser.parse_args()

manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
name = manifest["package"]["name"]
version = manifest["package"]["version"]
if SEMVER.fullmatch(version) is None:
    fail(f"Cargo.toml package.version is not strict SemVer: {version!r}")

lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
root_packages = [
    package
    for package in lock.get("package", [])
    if package.get("name") == name and package.get("source") is None
]
if len(root_packages) != 1 or root_packages[0].get("version") != version:
    fail("Cargo.lock root package version does not exactly match Cargo.toml")

main_source = (ROOT / "src" / "main.rs").read_text(encoding="utf-8")
gui_source = (ROOT / "src" / "gui.rs").read_text(encoding="utf-8")
expected_constant = 'pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");'
if expected_constant not in main_source:
    fail("application VERSION must come directly from CARGO_PKG_VERSION")
if "crate::VERSION" not in gui_source:
    fail("GUI must display the shared application VERSION constant")
for path, source in (("src/main.rs", main_source), ("src/gui.rs", gui_source)):
    if re.search(r"xus-miner v?[0-9]+\.[0-9]+\.[0-9]+", source, re.IGNORECASE):
        fail(f"hard-coded application version found in {path}")

tag = args.tag
if tag is None and os.environ.get("GITHUB_REF_TYPE") == "tag":
    tag = os.environ.get("GITHUB_REF_NAME")
if tag is not None and tag != f"v{version}":
    fail(f"tag {tag!r} must equal 'v{version}'")

print(version)
