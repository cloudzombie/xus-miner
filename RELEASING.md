# Release and version contract

`Cargo.toml` `package.version` is the single version source. Rust embeds it via
`CARGO_PKG_VERSION`; the GUI footer and `xus-miner --version` use that same
constant. `Cargo.lock` and a release tag must agree exactly.

The only valid tag for application version `X.Y.Z` is `vX.Y.Z`. The release
tooling accepts no tag/version input and maintainers must never create `v*` tags
by hand. Repository rules block all `v*` tag movement/deletion with no bypass,
and tag-push CI rejects a version mismatch. Tags are never reused. If a release
is wrong, fix it and increment the patch version; never rewrite a published
version.

## Release procedure

1. Create a branch from current `main`.
2. Change only `package.version` in `Cargo.toml` and the human release notes.
3. Run `cargo check` once to update the root package entry in `Cargo.lock`, then
   inspect the lockfile diff. Dependency changes do not belong in an incidental
   version bump.
4. Run the complete locked validation sequence from `AGENTS.md`, plus
   `python3 scripts/check_version.py`.
5. Merge through a pull request only after every protected check passes.
6. From GitHub Actions, manually dispatch the `Release` workflow on `main`.
   Never create or push a release tag by hand.

The workflow accepts no version input: it derives `vX.Y.Z` from `Cargo.toml`,
refuses reused versions or any ref other than current `main`, rebuilds and tests
on Linux, Apple Silicon macOS, Intel macOS, and Windows, and confirms each
compiled binary reports the exact same version. Only after those gates does it
create the matching tag, checksums, provenance attestations, and GitHub release.
Only that final source-free publish job receives a repository-scoped write
token; all source/build jobs remain read-only. The tag reference is created
atomically at the already-verified `main` SHA and read back before the release
is published. The release is assembled as a draft and published only after every
asset is attached, allowing GitHub's repository-level immutable-release policy
to lock the final tag and assets and issue its release attestation. A tag-push CI
check independently rejects any version mismatch for tags created outside the
release workflow. If publishing is interrupted, a rerun may replace only the
matching incomplete draft; it still refuses any already-published release.
