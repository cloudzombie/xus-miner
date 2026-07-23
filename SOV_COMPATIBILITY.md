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

The XUS Miner 0.1.1 compatibility baseline is the current SOV
`release/v0.1.99` branch at
`752b75bb9dcfa6392136fae3faad1ec515719c2a`. At validation time that SOV branch
had not yet been published as an immutable `v0.1.99` tag. Its mining RPC and
header-wire files are byte-identical to the preceding contract; its mainnet
preset adds nonzero miner signaling bits, which the miner preserves because it
changes only the trailing nonce. Recheck the contract hash when SOV `v0.1.99`
is finally tagged.

The connected SOV node normally listens on TCP port `8645`. The miner uses:

- `sov_health` — required; supplies current `height` and `mempool` size.
- `sov_getBlockTemplate` — required; supplies template ID, serialized header
  blob, nonce offset, inclusive 256-bit target, PoW algorithm/key, height,
  timestamp, and encoded proposer/coinbase account.
- `sov_submitBlock` — required; submits the template ID, nonce, and timestamp
  after a locally valid network-target seal is found. The result counts as an
  accepted block only when it contains `accepted: true`, the mined height, and
  the exact locally computed sealed-header hash.
- `sov_getDifficulty` — optional dashboard enrichment for estimated network
  hashrate and target block cadence. Failure never blocks job installation.
- `sov_getPeerInfo` — optional dashboard enrichment. Only the authenticated
  integer `peers` count is displayed; addresses and versions are not retained.
  Failure clears the chip to unavailable and never blocks job installation.

The miner decodes the canonical Borsh header and refuses to hash when the RPC
height, timestamp, proposer, template ID, nonce position, algorithm, or target
is inconsistent with the encoded work. It independently decodes the header's
Bitcoin-style compact `bits` commitment and requires the advertised 256-bit
target to match it exactly. Direct template blobs are bounded, the PoW key must
be exactly 32 bytes, and SOV 0.1.99's `versionBits = 3` signaling vector is
covered by a fixed regression test. Complete direct-RPC replies are also
strictly bounded (including headers and the hex-expanded template), and required
node calls fail the session after five seconds instead of leaving stale work
presented as live indefinitely.

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
