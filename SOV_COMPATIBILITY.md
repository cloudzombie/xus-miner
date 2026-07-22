# SOV compatibility requirements

XUS Miner is an external network client. It requires no writable SOV checkout,
node database, wallet file, seed phrase, or private key.

## Compile-time boundary

There is no SOV source dependency. `Cargo.toml` contains no SOV Git package,
local path, workspace member, submodule, or symlink. Client-side sealing uses
the locked upstream `randomx-rs` and `sha2` crates; block-template identity uses
locked `blake3`; and the minimal read-only header wire structure uses `borsh`.

The repository carries fixed templates and expected identifiers captured from a
real SOV node. They prove the independent wire implementation remains compatible
without importing chaincode. A SOV protocol change must be handled by updating
the documented client contract and adding a new fixed compatibility vector in
this repository. Never patch SOV from here; a required chain change is a
separate, explicitly authorized SOV task.

## Direct-node JSON-RPC contract

The connected SOV node normally listens on TCP port `8645`. The miner uses:

- `sov_health` — required; supplies current `height` and `mempool` size.
- `sov_getBlockTemplate` — required; supplies template ID, serialized header
  blob, nonce offset, inclusive 256-bit target, PoW algorithm/key, height,
  timestamp, and encoded proposer/coinbase account.
- `sov_submitBlock` — required; submits the template ID, nonce, and timestamp
  after a locally valid network-target seal is found.
- `sov_getDifficulty` — optional dashboard enrichment for estimated network
  hashrate and target block cadence. Failure never blocks job installation.

The miner decodes the canonical Borsh header and refuses to hash when the RPC
height, timestamp, proposer, template ID, nonce position, algorithm, or target
is inconsistent with the encoded work.

For a node on another machine, SOV Station must expose RPC on the LAN and the
host firewall must allow the chosen port. Station's internal mining toggle is
independent of this external miner.

## Stratum contract

Pool mode uses newline-delimited JSON over TCP with `login`, `job`, `submit`,
and `keepalived` messages. A job must provide a full 32-byte share target,
eight-byte trailing nonce location, supported algorithm, height, and RandomX
seed when applicable.

A variable-difficulty share target is not a consensus block target. Block ETA
and round probability remain unavailable unless the bridge separately supplies
`network_target_full` or `network_target`. A pool-accepted share must never be
presented as proof that this miner found a network block.
