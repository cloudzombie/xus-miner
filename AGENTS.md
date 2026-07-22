# XUS Miner repository boundary

This repository owns only the standalone XUS Miner client.

- Work only inside this repository. Never edit, patch, stage, commit, push, or
  run a write-capable tool against the SOV chain repository or another sibling
  repository from an XUS Miner task.
- Never introduce a local path dependency, symlink, submodule, workspace
  membership, or generated write path into a SOV checkout.
- Do not add SOV source dependencies of any kind. Compatibility is established
  through the documented RPC/Stratum contract and fixed known-answer vectors.
  Third-party crates must use exact versions and the committed `Cargo.lock`.
- Runtime integration with a node is limited to documented RPC or Stratum
  network calls. The miner must not read or write node source, chain state, a
  wallet seed, or private keys.
- If a miner change appears to require chaincode changes, stop and ask the user
  to authorize a separate task in the SOV repository. Do not cross the boundary
  from this repository.

Run `python3 scripts/check_chaincode_boundary.py` before publishing changes.

## Build and validation quick reference

Read `BUILDING.md` and `SOV_COMPATIBILITY.md` before changing dependencies,
wire parsing, PoW, RPC, or Stratum behavior.

The required pre-publish sequence is:

```sh
python3 scripts/check_chaincode_boundary.py
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --bin xus-miner
cargo test --locked --test miner_protocol
cargo audit --deny warnings
cargo build --locked --release
```

The ignored RandomX equivalence test allocates a full dataset and is not part of
the ordinary loop. Run it deliberately, one test thread at a time, on a machine
with at least 3 GiB of free memory.
