# Changelog

## 0.1.6

- **Block-time graph** (the release centerpiece): a new GUI card plots the
  interval between each confirmed block's miner-declared header timestamp
  and its parent's (`sov_getBlockByHeight` `header.timestamp_ms`,
  live-verified in milliseconds), one bar per block in a bounded
  newest-right window of up to 32 intervals, with dashed reference lines at
  1× and 2× the node's own `sov_getDifficulty` `targetBlockMs` — no cadence
  is hardcoded and no SOV consensus rule is encoded in the miner.
  - Honest about miner-declared clocks: a zero or NEGATIVE interval is a
    real non-monotonic timestamp pair and renders as a distinct baseline
    "≤0" marker (never clamped, never dropped); an interval whose endpoint
    the node did not disclose renders as a hollow "?" gap (never
    interpolated or carried forward). Both cases are unit-tested.
  - Your accepted blocks are marked by the ★ shape (not hue alone) on the
    existing sealed-hash proof standard; bar-height-vs-reference-line is the
    primary encoding, colour is secondary.
  - Stable axis (next 1/2/5×10ⁿ ceiling), monospace numerics, per-bar
    tooltips limited to node-supplied fields (height, interval, declared
    timestamp, tx count, yours/network), a rolling window median/mean over
    KNOWN intervals only (exclusions stated, never substituted), and a live
    "SINCE LAST BLOCK (OBSERVED)" readout derived from the local observation
    clock — explicitly not from miner-declared timestamps.
  - Data flows through the existing block-flow cache, widened to a 33-block
    window; backfill remains capped at 4 `sov_getBlockByHeight` calls per
    refresh under a 2-second budget, so a cold start fills gradually and the
    graph can never issue unbounded RPCs. Pool/Stratum mode hides the graph
    exactly like the strip (no direct node RPC); a reorg refetch corrects
    the affected bars instead of duplicating them.
- **Network block feed**: every block that appears on the network — not just
  this miner's accepted submissions — now produces a timestamped, clearly
  marked line on all three surfaces: the headless stderr log, the
  `--json-events` stream (new stable `chain_baseline` / `chain_block` /
  `chain_gap` / `chain_reorg` / `chain_feed_unavailable` events; the
  existing `share`/`job`/`block_flow` events are untouched), and the GUI
  engine log (same 500-line/8-KiB bounds as before).
  - Session start or reconnect logs a single "BLOCK FEED live at height N"
    baseline — chain history is never replayed as a flood of block-found
    lines, and a height+hash already reported is never reported twice,
    including across reconnects.
  - A tip jump reports at most 4 blocks individually plus an explicit
    "… and N more" summary, bounding both log lines and follow-up RPCs.
  - The same height reappearing with a different node-disclosed hash logs a
    distinctly worded REORG line, never a duplicate block-found line.
  - YOURS is claimed strictly on the existing accepted-submission proof
    standard (exact sealed-hash match; accepted-height fallback only where
    the node discloses no hash). Pool/Stratum mode states explicitly that
    the feed is unavailable — a pool share is never presented as a network
    block.
- GUI honesty fixes from an independent visual audit:
  - TX QUEUE is now three-way: "CLEAR" only on a node-reported zero,
    "UNAVAILABLE" when the node never supplied a mempool size (previously
    absent data read as an affirmative "CLEAR").
  - The headline HASHRATE card is gated by the same telemetry-freshness
    proof as the ETA and heartbeat: "—" when no engine runs, and an
    explicit muted "STALE" marker when a running engine stops reporting —
    a wedged engine can no longer keep its last number painted as live.
  - The prominent height readouts are relabeled "MINING BLOCK": the value is
    the template height being sealed (tip + 1), not the confirmed chain
    height an explorer shows.
- CI/Release: the one-worker optimized-recovery and one-worker portable
  light-memory RandomX soaks now run on the Apple Silicon (macos-15) leg as
  well as Windows, closing the standing directive that every build runs
  Apple-Silicon-specific tests rather than build-only.

## 0.1.5

- Complete the mempool.space-style block-flow strip by consuming the SOV
  node's new v0.1.99 parity RPCs, with strict graceful degradation: these
  RPCs are built but not yet deployed to the live network, so every consumer
  falls back to its honest placeholder on a pre-upgrade node (verified by a
  dedicated legacy-node integration test) and lights up automatically once
  nodes upgrade. No rendered number is ever invented.
  - **Projected pending blocks** from the new `sov_getMempoolHistogram`:
    effective-tip fee buckets (decimal-string grains, highest first) packed
    into blocks by the node's own `maxBlockTxs` capacity — the blockspace
    auction's real per-block unit. The first packed block is the forming
    template block (already rendered from real template data), so only the
    overflow beyond it appears as projected tiles, each showing its real
    transaction count and effective-tip range; the tail past the render cap
    merges into the last tile so nothing pending is dropped. An old node
    (method not found), a malformed reply, or an empty mempool renders NO
    projected tiles — the honest state.
  - **FEE TO GET IN** now uses `sov_estimateFee`'s new `floorGrains` — the
    live next-block auction floor — when the node serves it, falling back to
    the flat `feeGrains` estimate on a pre-upgrade node.
  - The **forming tile's transaction count** reads the template's new
    `txCount`/`txIds` fields; a pre-upgrade template (txRoot only) keeps the
    honest "—".
  - The **★ YOURS highlight** is exact wherever the upgraded
    `sov_getBlockByHeight` discloses `header.hash` (hash identity is
    authoritative, so a same-height reorg can never mislabel someone else's
    block); the accepted-height fallback remains for pre-upgrade nodes.
  - The mock node in `tests/rpc_0199_protocol.rs` serves the exact upgraded
    reply shapes (pinning the real contract) and a legacy pre-upgrade mode;
    tests cover both paths end to end, including histogram → projected-tile
    packing, floor-over-fee preference, and every missing-RPC fallback.
- Relabel the PoW flight recorder to make its fixed rolling window obvious:
  the header now reads `POW FLIGHT RECORDER • ROLLING LAST <span> (OLDEST
  DROPS OFF)` instead of a `LAST <span>` that plateaued like a frozen uptime
  counter once the ~6-minute sample buffer filled. Pure label change; the
  sampling logic and bounds are untouched.

## 0.1.4

- **Fix a field GUI crash present in v0.1.3** (captured by the v0.1.3 crash
  log on a live macOS session, platform-independent): the live-meter's
  height-change markers used `bool::then_some(height.expect(...))`, whose
  argument is evaluated **eagerly** — so one meter sample with no height
  (the engine legitimately reports `height: null` between jobs, after an
  accepted block, and while reconnecting) panicked the dashboard paint path
  (`height checked`) and aborted the whole application on the next repaint.
  The marker derivation is now lazy and total (`filter`/`map`, extracted as
  a pure helper), with a regression test that replays the previously fatal
  jobless-sample history through the real paint path.
- Add a mempool.space-style **block-flow strip** to the GUI dashboard for
  direct-node sessions: the forming template block, a tip divider, and the
  most recent confirmed blocks as tiles with their real heights and
  transaction counts — the length of the `transactions` array that
  `sov_getBlockByHeight` returns (live-verified shape:
  `{"header": {...}, "transactions": [...]}`; a `sov_getBlockDigest`-style
  `txIds` array is accepted equivalently). The live block template exposes
  NO transaction list or count (only its `txRoot` commitment), so the
  forming tile honestly shows "—" for its count — a real data gap, not a
  fabricated number. Blocks this miner sealed — accepted `sov_submitBlock`
  submissions whose confirmed hash the node echoed — are highlighted,
  matched by block hash when a response discloses one and by accepted
  height otherwise (the live shape discloses no block hash). A reorg
  detected by a changed hash invalidates every cached descendant so stale
  tiles are refetched instead of shown.
- Add mempool-pressure and fee-to-get-in chips: pending-transaction count
  from the node (`sov_health` every poll, refreshed by `sov_getMempoolSize`,
  whose live reply is a bare integer) and the node's `sov_estimateFee`
  `feeGrains` value (live shape: a decimal string in grains), rendered in
  XUS (1 XUS = 10^8 grains) — the "fee ≥ X to make the next block"
  indicator of the v0.1.98 blockspace auction. A nonzero fee below display
  precision renders as "<0.00001 XUS", never as free.
- Every rendered number is a real node RPC value; anything a node does not
  supply (older node, missing RPC, failed or slow call) renders as the
  neutral placeholder "—", never an invented or estimated-looking value.
  All new calls are read-only, optional, strictly bounded (1-second
  timeouts, at most four block fetches per template refresh inside a
  2-second budget), run on the existing template-refresh cadence after work
  installation, and can never delay hashing.
- Documented seam, deliberately NOT built: mempool.space's several projected
  pending blocks bucketed by fee rate require a mempool fee-histogram RPC
  that the SOV node does not expose today (`sov_getMempoolSize` returns one
  count, `sov_estimateFee` one estimate). Exposing a histogram is an
  additive node change owned by the SOV repository; when it exists, extend
  the engine's `block_flow` telemetry and render the projected tiles beside
  the single real forming tile (see `src/blockflow.rs`).

## 0.1.3

- Add persistent crash-diagnostic logging for both the GUI and the isolated
  engine: timestamped, immediately flushed log files under
  `~/.xus-miner/logs/` (`%USERPROFILE%\.xus-miner\logs\` on Windows), pruned to
  the ten newest files, with the active path printed on stderr at startup and
  verbosity controlled by `XUS_MINER_LOG` (`error`/`warn`/`info`/`debug`;
  default `info`). Every line self-identifies the miner version, operating
  system, and architecture, and the startup header records detected CPU
  features, so one pasted log is diagnosable without guessing the platform.
- Install a panic hook that writes the panic location, message, and a full
  forced backtrace to that log file before the process dies — a GUI-spawned
  engine has no visible stderr on Windows or macOS, so the file is the only
  record of a field crash. The hook is plain safe Rust (no signal handlers,
  no new unsafe surface) and chains to the default stderr hook.
- Log the RandomX engine lifecycle: decoded flag sets (JIT/HARD_AES/Argon2
  SIMD vs portable DEFAULT), synchronous full-dataset initialization start,
  duration, and failure, light-cache fallbacks, each worker's engine mode,
  worker panic recoveries, memory-preflight decisions, and every GUI
  supervision event (engine spawn with PID, unexpected exit status, bounded
  restart scheduling with reason, quarantine, operator Stop). The macOS
  `memory_pressure` helper logs each probe outcome at debug and every failure
  or deadline overrun at warn.
- Add a multi-worker RandomX lifecycle regression: four threads share one
  seed manager, hash concurrently, switch seeds together, and return to a
  prior seed while asserting every digest, pinning the shared-allocation
  ownership contract that the two-worker engine relies on.

- Remove `sysinfo` from every macOS build after macOS 26.5.1 reported that its
  `host_statistics64` request used the macOS 27 `vm_statistics64` count and
  could corrupt process memory. The GUI and headless memory gates now share a
  bounded parser around Apple's absolute-path `/usr/bin/memory_pressure -Q`
  utility, launched without an inherited environment or application FFI.
- Add strict parser coverage plus a repeated live macOS memory-probe test, and
  make CI and Release fail if `sysinfo` re-enters either macOS dependency tree.
- Assert that the primary macOS runner is native `arm64`, publish its archive
  explicitly as `macos-apple-silicon-arm64`, and run the two-worker direct-RPC
  RandomX stability soak on Apple Silicon as well as Windows.
- Preserve the fast shared-dataset RandomX engine and configured worker count
  for ordinary launches; the macOS GUI-memory fix does not disable JIT or
  override the selected worker count.
- Keep the GUI alive when an isolated engine exits unexpectedly and attempt up
  to five supervised restarts with bounded exponential backoff. On Windows,
  every recovery child uses one effective worker without changing the saved
  preference. Unknown failures retry the optimized backend once.
- Send a repeated one-worker failure, native access violation, illegal
  instruction, heap corruption, fail-fast, virtual-memory, resource,
  pagefile-quota, or commitment-limit status directly to one-worker portable
  light mode without allocating the full dataset. Missing-DLL failures stop
  automatically because retrying cannot repair the runtime environment.
- Remove the unusably slow `FLAG_DEFAULT` full-dataset initialization retry;
  an observable optimized cache/dataset allocation failure now enters light
  fallback promptly instead of starting that minutes-long retry.
- Make Stop permanently suppress any pending recovery for that mining session,
  including exit/drain races, and quarantine failed-start child handles until
  termination is proven so a second full-dataset engine cannot start beside an
  unobserved first process.
- Extend the Windows release gate to run separate 90-second RandomX soaks for
  two-worker optimized, one-worker optimized recovery, and one-worker portable
  light-memory operation.
- Bound macOS memory-helper stdout/stderr while draining it, enforce a 500 ms
  process deadline, and move periodic probes off the GUI thread.
- Run GUI-spawned Windows engines below normal priority, and add a flushed
  parent-pipe heartbeat plus a subprocess regression so a terminated GUI cannot
  leave an invisible orphan miner running.

## 0.1.2

- Isolate the exact, locked `randomx-rs` native library behind a narrow reviewed
  ownership adapter that checks every cache, dataset, and VM allocation before
  hashing. Full-dataset initialization is synchronous, so a failed operating
  system thread spawn cannot strand native pointer workers.
- Share one read-only full RandomX dataset across all worker VMs instead of
  allocating approximately 2.3 GiB per worker. Memory preflight now budgets one
  dataset plus a conservative per-worker allowance.
- Fall back to safe light-memory RandomX after fast-mode allocation failure,
  retry fast mode after memory is released, and supervise recoverable Rust
  worker panics without terminating the mining engine.
- Bound child-process output queues and log lines, drain final stdout/stderr
  before retaining sanitized crash reports, and explain common Windows native
  exit codes without recording the Stratum password or endpoint credentials.
  If process termination cannot be proven, quarantine its handle and keep
  restart disabled rather than risk launching a second memory-heavy engine.
- Report each worker's actual fast-shared or light-fallback mode, keep GUI worker
  health synchronized with error/recovery events, preserve truthful node-link
  and degraded-hashing indicators, and return a failing process status when an
  engine thread cannot be started.
- Validate the real shared full-dataset path on every release platform and add
  a two-worker Windows direct-RPC stability soak.

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
