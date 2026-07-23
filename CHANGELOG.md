# Changelog

## 0.1.1

- Add a compact authenticated SOV node-peers chip backed by the optional
  `sov_getPeerInfo` RPC.
- Explain the 48 recent activity slices in a persistent, resizable meter
  legend: cyan intensity represents sampled hashrate and green records a sample
  whose mempool contained transactions.
- Scan only native RAM counters every ten seconds and immediately before Start;
  show available/total memory, calculate a conservative safe worker limit, and
  block unsafe RandomX starts without silently changing the configured count.
  The isolated/headless engine independently enforces the same policy before its
  first RandomX job.
- Validate the current SOV `release/v0.1.99` block-template contract, including
  nonzero mainnet signaling bits, exact 32-byte PoW key, bounded Borsh header,
  trailing nonce, and compact `bits` to full-target agreement.
- Treat direct block submission as accepted only after the node returns
  `accepted: true` with the expected height and locally authenticated sealed
  block hash; stop hashing that completed template immediately and clear its
  visual ACTIVE state with identity-safe telemetry plus a 30-second green,
  pulsing `BLOCK FOUND` confirmation.
- Bound complete direct-RPC responses and use a five-second required-call
  timeout so a malformed or stalled configured node cannot grow memory without
  limit or leave a stale connection presented as live for thirty seconds.

## 0.1.0

- Initial standalone native GUI and headless SOV/XUS CPU miner release.
