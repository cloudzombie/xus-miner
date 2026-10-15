# SOV compatibility requirements

XUS Miner is an external network client. It requires no writable SOV checkout,
node database, wallet file, seed phrase, or private key.

## Compile-time boundary

There is no SOV source dependency. `Cargo.toml` contains no SOV Git package,
local path, workspace member, submodule, or symlink. Client-side sealing uses
the locked upstream `randomx-rs` native library through the miner's isolated
allocation-checked adapter, plus `sha2`; block-template identity uses locked
`blake3`; and the minimal read-only header wire structure uses `borsh`.

The repository carries fixed templates and expected identifiers captured from a
real SOV node. They prove the independent wire implementation remains compatible
without importing chaincode. A SOV protocol change must be handled by updating
the documented client contract and adding a new fixed compatibility vector in
this repository. Never patch SOV from here; a required chain change is a
separate, explicitly authorized SOV task.

## Direct-node JSON-RPC contract

XUS Miner 0.1.3 retains the 0.1.1 compatibility baseline for the current SOV
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
- `sov_getBlockByHeight` — optional, read-only, named params
  (`{"height": N}`); supplies confirmed-block detail for the block-flow
  strip, the network block feed, and the block-time graph: the
  `transactions`/`txIds` count, the miner-declared `header.timestamp_ms`
  (integer milliseconds; live-verified — the camelCase `timestampMs` is
  accepted equivalently), and, on upgraded nodes, `header.hash`. Absent
  fields render as explicit placeholders/gaps; nothing is interpolated.
- `sov_getMempoolSize`, `sov_estimateFee`, `sov_getMempoolHistogram` —
  optional dashboard/strip enrichment (live-verified shapes; the histogram is
  an upgraded-node addition). All fail soft to honest placeholders. The
  histogram reply is strictly validated (bounded bucket list, positive
  `maxBlockTxs`, buckets highest-tip-first, bucket counts summing exactly to
  `txCount`); a reply failing any check degrades identically to an absent
  RPC. The `block_flow` telemetry event carries the validated summary in an
  ADDITIVE `backlog` member (`tx_count`, `max_block_txs`, exact
  `depth_blocks`, packed `blocks`), which is JSON `null` whenever the node
  did not serve a valid histogram — consumers can therefore always tell "the
  node did not say" apart from an affirmatively empty mempool.

### Network block feed and block-time graph

Every required `sov_health` poll (one per ~2-second cycle) reads the tip
height; the tip-advance detector itself runs inside the template-refresh
branch — entered on session start, whenever the tip height changed since the
last refresh, and at latest every 30 seconds. Because any tip change forces
that branch, every tip movement is observed; an unchanged tip is simply a
no-op for the feed either way. Each block that appears on the network
produces one timestamped stderr line, one stable `chain_block` JSON event,
and one GUI log entry, marked YOURS strictly on the accepted-submission
proof standard (exact sealed-hash match, or the accepted height where the
FETCHED block detail discloses no hash). A per-block detail fetch that fails
or misses its budget is a distinct third state: the hash is then unknown —
not "undisclosed by the node" — so every surface says the fetch failed, and
a height match the failure left uncorroborated is reported as
possibly-yours-unverified, never as an unqualified YOURS. The `chain_block`
event carries that distinction in additive fields (`detail_fetch_failed`,
`mine_unverified`, `mine_basis`) without changing any pre-existing field;
`block_flow` tiles likewise add `mine_basis`. Detail comes only from bounded
`sov_getBlockByHeight` calls: at most `MAX_GAP_BLOCKS` (4) per tip advance
under a wall-clock budget for the feed, plus at most 4 strip-backfill calls
per refresh — a bounded worst case of 8 detail calls per template refresh. A
larger jump is summarized explicitly ("… and N more") instead of flooding
lines or RPCs.
Session start or reconnect announces a single baseline and never replays
chain history. The block-time graph derives every bar from consecutive
miner-declared `header.timestamp_ms` values and reference-marks plain
multiples of the node-supplied `targetBlockMs`; no cadence is hardcoded and
no SOV consensus rule is encoded in this client.

The miner decodes the canonical Borsh header and refuses to hash when the RPC
height, timestamp, proposer, template ID, nonce position, algorithm, or target
is inconsistent with the encoded work. It independently decodes the header's
Bitcoin-style compact `bits` commitment and requires the advertised 256-bit
target to match it exactly. Direct template blobs are bounded, the PoW key must
be exactly 32 bytes, and SOV 0.1.99's `versionBits = 3` signaling vector is
covered by a fixed regression test. Complete direct-RPC replies are also
strictly bounded (including headers and the hex-expanded template), and required
node calls share a five-second deadline across address connection attempts,
request writes, and response reads. Progress on a trickled header or body does
not reset that deadline. Hostname resolution still uses the operating system's
blocking resolver; use an IP endpoint when a DNS-independent deadline is needed.
Responses with `Content-Length` complete as soon as that many bytes arrive,
without waiting for the node to close the connection. Close-delimited HTTP/1.0
and HTTP/1.1 replies remain supported under the same read deadline.

The transport checks the HTTP success status, bounded headers, unambiguous
decimal `Content-Length`, and the JSON-RPC `2.0` envelope with the exact numeric
request ID before using any result. Duplicate lengths, truncated bodies,
unsupported transfer/content encodings, and mismatched response IDs fail closed.
Chunked and compressed responses remain unsupported. Required-call failures
clear installed work and reconnect; optional enrichment continues to fail soft.

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

Pool mode has no direct node RPC, so the network block feed and block-time
graph are unavailable there: the client says so explicitly (one
`chain_feed_unavailable` event and log line per session) rather than
presenting pool shares as network blocks.
