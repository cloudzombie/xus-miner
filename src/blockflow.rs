//! Read-only "block flow" telemetry: a mempool.space-style strip of recent
//! confirmed blocks, the forming template block, and the fee-to-get-in
//! indicator, derived exclusively from real SOV node RPC responses
//! (`sov_getBlockByHeight`, `sov_getBlockTemplate`, `sov_getMempoolSize`,
//! `sov_estimateFee`). A datum a node does not supply stays `None` and is
//! rendered as a neutral placeholder; nothing here invents, estimates, or
//! fabricates a value.
//!
//! Projected pending blocks (mempool.space's multi-block view) come from the
//! additive `sov_getMempoolHistogram` RPC: effective-tip fee buckets, highest
//! first, packed into blocks by the node's own `maxBlockTxs` capacity. An
//! older node without the RPC — or an empty mempool — yields NO projected
//! blocks; that honest absence renders as no tiles, never invented ones.

use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::VecDeque;

/// Confirmed blocks kept in the recent-window cache (newest first). 33
/// consecutive block timestamps yield the 32 block-time intervals of the
/// GUI's block-time graph (a bounded, newest-right window); the flow strip
/// renders only its own smaller tile cap from the same cache. Backfill stays
/// rate-limited by `MAX_FETCH_PER_REFRESH`, so widening the window can never
/// widen the per-refresh RPC load — a cold start simply fills across cycles.
pub(crate) const RECENT_DEPTH: u64 = 33;
/// Upper bound on `sov_getBlockByHeight` calls per template refresh.
pub(crate) const MAX_FETCH_PER_REFRESH: usize = 4;
/// Accepted block submissions remembered for the "yours" highlight.
const MAX_SEALED_TRACKED: usize = 32;
/// Sanity bound for a hash string taken from an RPC response.
const MAX_HASH_CHARS: usize = 128;
/// Projected pending blocks rendered beside the forming tile; the packing
/// tail beyond this cap merges into the last projected block (as
/// mempool.space does) so nothing pending is silently dropped.
pub(crate) const MAX_PROJECTED_BLOCKS: usize = 6;
/// Most individually reported blocks per tip advance. A larger jump (slow
/// poll, node catch-up) reports only the newest `MAX_GAP_BLOCKS` heights plus
/// one explicit "… and N more" summary, so a big gap can never spam unbounded
/// log lines or issue unbounded `sov_getBlockByHeight` calls.
pub(crate) const MAX_GAP_BLOCKS: u64 = 4;
/// Reported height→hash entries remembered for idempotence and reorg
/// detection (bounded; oldest heights are pruned as the tip advances).
const MAX_REPORTED_TRACKED: usize = 64;

/// The object that actually carries block fields: some RPC shapes nest the
/// block under a `block` key; use it when present, else the value itself.
fn block_facet(value: &Value) -> &Value {
    match value.get("block") {
        Some(inner) if inner.is_object() => inner,
        _ => value,
    }
}

/// Real transaction count of a block response. The live SOV node's
/// `sov_getBlockByHeight` returns `{"header": {...}, "transactions": [...]}`
/// (verified against mainnet), so the count is the length of `transactions`.
/// `sov_getBlockDigest`'s `txIds` array is accepted as the equivalent lighter
/// shape. Absent both, the count is unknown and the UI must show a
/// placeholder — never a guess.
///
/// NOTE: pre-upgrade `sov_getBlockTemplate` replies expose NO transaction
/// list or count (only `txRoot`), so for those the forming template block
/// yields `None` and the tile honestly renders "—". The upgraded node's
/// template includes `txCount` + `txIds`, which this reads automatically.
pub(crate) fn tx_count(value: &Value) -> Option<u64> {
    let facet = block_facet(value);
    if let Some(transactions) = facet.get("transactions").and_then(Value::as_array) {
        return Some(transactions.len() as u64);
    }
    if let Some(ids) = facet.get("txIds").and_then(Value::as_array) {
        return Some(ids.len() as u64);
    }
    // Upgraded-node block template / digest: an explicit integer count.
    facet.get("txCount").and_then(Value::as_u64)
}

/// The block's own hash, when the node discloses one. The live
/// `sov_getBlockByHeight` shape nests fields under `header` and does not
/// return a top-level `hash`; accept `hash` at the top level, inside `block`,
/// or inside `header` so any disclosing shape is used, else `None` (the
/// "yours" highlight then falls back to the accepted height).
pub(crate) fn block_hash(value: &Value) -> Option<String> {
    let facet = block_facet(value);
    facet
        .get("hash")
        .or_else(|| facet.get("header").and_then(|header| header.get("hash")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|hash| !hash.is_empty() && hash.len() <= MAX_HASH_CHARS)
        .map(str::to_owned)
}

/// The block's own miner-declared header timestamp in milliseconds, when the
/// node discloses one. Live-verified `sov_getBlockByHeight` shape: the field
/// is `header.timestamp_ms` (integer milliseconds); the node's camelCase
/// convention `timestampMs` (used by `sov_getBlockTemplate`) is accepted
/// equivalently. Absent both, the timestamp is unknown and every consumer
/// must render a gap — never interpolate, estimate, or carry a neighbour
/// forward.
pub(crate) fn block_timestamp_ms(value: &Value) -> Option<u64> {
    let facet = block_facet(value);
    let read = |object: &Value| {
        object
            .get("timestamp_ms")
            .or_else(|| object.get("timestampMs"))
            .and_then(Value::as_u64)
    };
    facet.get("header").and_then(read).or_else(|| read(facet))
}

/// The fee-to-get-in from `sov_estimateFee`, in grains. The live node returns
/// `{"feeGrains":"1651760","gasPriceGrains":"10","gasUsed":...,"kind":...}`
/// (verified against mainnet): `feeGrains` is a decimal string. A bare
/// integer, or an integer `feeGrains`, is also accepted. Anything else yields
/// `None` (placeholder), never a guess.
pub(crate) fn fee_grains(value: &Value) -> Option<u64> {
    if let Some(fee) = value.as_u64() {
        return Some(fee);
    }
    grains(value.get("feeGrains")?)
}

/// A grains amount the node encodes either as an integer or (its live
/// convention for large amounts) a decimal string.
fn grains(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
    })
}

/// The fee-to-get-in for the strip: the upgraded node's `sov_estimateFee`
/// additionally returns `floorGrains` — the live next-block auction floor —
/// which is the real "fee to get in" and is preferred when present. An older
/// node without it falls back to the flat `feeGrains` estimate; neither
/// present yields `None` (placeholder), never a guess.
pub(crate) fn fee_floor_grains(value: &Value) -> Option<u64> {
    value
        .get("floorGrains")
        .and_then(grains)
        .or_else(|| fee_grains(value))
}

/// One effective-tip fee bucket of `sov_getMempoolHistogram`, exactly as the
/// node serves it (`feeRateGrains` is a decimal-string grains rate; buckets
/// arrive highest-tip first, capped at 128 with the low-tip tail merged).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistogramBucket {
    pub(crate) fee_rate_grains: u64,
    pub(crate) tx_count: u64,
}

/// The parsed `sov_getMempoolHistogram` reply. `None` for any malformed or
/// missing field: an old node (method not found) or a bad reply degrades to
/// "no projected blocks", never to invented ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MempoolHistogram {
    pub(crate) tx_count: u64,
    pub(crate) max_block_txs: u64,
    pub(crate) buckets: Vec<HistogramBucket>,
}

pub(crate) fn mempool_histogram(value: &Value) -> Option<MempoolHistogram> {
    let tx_count = grains(value.get("txCount")?)?;
    let max_block_txs = grains(value.get("maxBlockTxs")?)?;
    let buckets = value
        .get("buckets")?
        .as_array()?
        .iter()
        .map(|bucket| {
            Some(HistogramBucket {
                fee_rate_grains: grains(bucket.get("feeRateGrains")?)?,
                tx_count: grains(bucket.get("txCount")?)?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(MempoolHistogram {
        tx_count,
        max_block_txs,
        buckets,
    })
}

/// One projected pending block: real bucketed mempool transactions packed by
/// the node's own per-block transaction capacity, with the effective-tip
/// range they span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectedBlock {
    pub(crate) tx_count: u64,
    pub(crate) fee_min_grains: u64,
    pub(crate) fee_max_grains: u64,
}

/// Pack the histogram's buckets (highest effective tip first, the node's
/// auction admission order) into blocks of `maxBlockTxs` transactions — the
/// capacity unit the node itself reports and the blockspace auction uses.
/// The FIRST packed block is the forming template block, which the strip
/// already renders from real template data, so only the blocks BEYOND it are
/// returned as projected tiles. Everything past `MAX_PROJECTED_BLOCKS`
/// merges into the last tile. An empty mempool, a zero capacity, or no
/// overflow beyond the forming block yields no projected tiles.
pub(crate) fn projected_blocks(histogram: &MempoolHistogram) -> Vec<ProjectedBlock> {
    if histogram.max_block_txs == 0 || histogram.tx_count == 0 {
        return Vec::new();
    }
    let mut blocks: Vec<ProjectedBlock> = Vec::new();
    let mut room = 0_u64;
    for bucket in &histogram.buckets {
        let mut remaining = bucket.tx_count;
        while remaining > 0 {
            if room == 0 {
                if blocks.len() > MAX_PROJECTED_BLOCKS {
                    // At the render cap: the whole tail merges into the
                    // last projected block instead of growing the packing.
                    let last = blocks.last_mut().expect("capped packing has blocks");
                    last.tx_count += remaining;
                    last.fee_min_grains = last.fee_min_grains.min(bucket.fee_rate_grains);
                    last.fee_max_grains = last.fee_max_grains.max(bucket.fee_rate_grains);
                    break;
                }
                blocks.push(ProjectedBlock {
                    tx_count: 0,
                    fee_min_grains: bucket.fee_rate_grains,
                    fee_max_grains: bucket.fee_rate_grains,
                });
                room = histogram.max_block_txs;
            }
            let take = remaining.min(room);
            let block = blocks.last_mut().expect("current packing block");
            block.tx_count += take;
            block.fee_min_grains = block.fee_min_grains.min(bucket.fee_rate_grains);
            block.fee_max_grains = block.fee_max_grains.max(bucket.fee_rate_grains);
            room -= take;
            remaining -= take;
        }
    }
    // Drop the forming block; keep only the projected overflow behind it.
    if blocks.len() <= 1 {
        return Vec::new();
    }
    blocks.split_off(1)
}

/// The pending-transaction count from `sov_getMempoolSize`. The live node
/// returns a bare integer (verified against mainnet); a recognized numeric
/// field of an object reply is tolerated as a fallback.
pub(crate) fn mempool_size(value: &Value) -> Option<u64> {
    if let Some(size) = value.as_u64() {
        return Some(size);
    }
    ["size", "count", "mempool", "pending"]
        .iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64))
}

/// Blocks this miner sealed itself: every entry is a locally computed sealed
/// header hash that the node's `sov_submitBlock` reply confirmed with
/// `accepted: true` — real accepted work, never an assumption.
#[derive(Debug, Default)]
pub(crate) struct SealedBlocks {
    entries: VecDeque<(u64, String)>,
}

impl SealedBlocks {
    pub(crate) fn record(&mut self, height: u64, hash: String) {
        self.entries.push_back((height, hash));
        while self.entries.len() > MAX_SEALED_TRACKED {
            self.entries.pop_front();
        }
    }

    /// Whether the block identified by `height` (and, when the node disclosed
    /// it, `hash`) is one of this miner's accepted submissions. A known block
    /// hash is authoritative: it alone decides, so a same-height reorg cannot
    /// mislabel someone else's block as ours. Only when the node did not
    /// disclose the block hash does the accepted height serve as the match.
    pub(crate) fn sealed(&self, height: u64, hash: Option<&str>) -> bool {
        match hash {
            Some(hash) => self.entries.iter().any(|(_, sealed)| sealed == hash),
            None => self.entries.iter().any(|(sealed, _)| *sealed == height),
        }
    }
}

/// How one tip poll advanced the network block feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TipAdvance {
    /// First successful poll of a session (start or reconnect): a single
    /// baseline is announced and chain history is never replayed as a flood
    /// of block-found lines.
    Baseline { height: u64 },
    /// The tip advanced. `report` holds the heights to announce individually
    /// (ascending, at most `MAX_GAP_BLOCKS`); `omitted` counts the older
    /// intervening heights (`omitted_from..=omitted_to`) summarized instead
    /// of reported one by one.
    Blocks {
        report: Vec<u64>,
        omitted: u64,
        omitted_from: u64,
        omitted_to: u64,
    },
    /// No new height (unchanged, or the tip moved backward — a reorg line is
    /// produced later by the hash change at refetch, never a duplicate
    /// block-found line here).
    Unchanged,
}

/// The outcome of registering one observed block with the feed's memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlockRecord {
    /// First time this height is reported: announce it.
    New,
    /// This height (and hash, when both are known) was already reported —
    /// including across reconnects. Never announce it again.
    AlreadyReported,
    /// The same height reappeared with a DIFFERENT node-disclosed hash: a
    /// reorg, announced distinctly, never as a duplicate block-found line.
    Reorg { previous_hash: String },
}

/// Tip-advance detector behind the network block feed. Pure state, fed only
/// real `sov_health` heights and real `sov_getBlockByHeight` hashes; it never
/// fabricates, estimates, or interpolates a block. It outlives RPC sessions so
/// a reconnect re-baselines without ever re-reporting a known height+hash.
#[derive(Debug, Default)]
pub(crate) struct TipWatch {
    /// Last tip height already accounted for; `None` until the first poll.
    last_tip: Option<u64>,
    /// Set by `begin_session`; forces a single baseline announcement on the
    /// next poll instead of replaying whatever happened while disconnected.
    needs_baseline: bool,
    /// Height → node-disclosed hash (when any) of every reported block,
    /// bounded to `MAX_REPORTED_TRACKED` newest heights.
    reported: BTreeMap<u64, Option<String>>,
}

impl TipWatch {
    /// Call at the start of every direct-RPC session (connect or reconnect):
    /// the next successful poll announces one baseline line only.
    pub(crate) fn begin_session(&mut self) {
        self.needs_baseline = true;
    }

    /// Feed one successful `sov_health` tip height; returns what to announce.
    pub(crate) fn observe_tip(&mut self, tip: u64) -> TipAdvance {
        if self.needs_baseline || self.last_tip.is_none() {
            self.needs_baseline = false;
            self.last_tip = Some(tip);
            // Remember the baseline height itself (hash still unknown) so a
            // later reorg replacing the baseline block is detected.
            self.reported.entry(tip).or_insert(None);
            self.prune();
            return TipAdvance::Baseline { height: tip };
        }
        let last = self.last_tip.expect("tip watch baselined");
        if tip <= last {
            // Unchanged tip, or a shorter chain after a reorg: track the
            // lower tip so a subsequent re-advance is observed, but announce
            // nothing here (idempotence; the reorg line comes from the hash
            // change at refetch).
            self.last_tip = Some(tip.min(last));
            return TipAdvance::Unchanged;
        }
        self.last_tip = Some(tip);
        let gap = tip - last;
        let reported = gap.min(MAX_GAP_BLOCKS);
        let omitted = gap - reported;
        let first_reported = tip - reported + 1;
        TipAdvance::Blocks {
            report: (first_reported..=tip).collect(),
            omitted,
            omitted_from: last + 1,
            omitted_to: first_reported.saturating_sub(1),
        }
    }

    /// Register a height about to be announced (from `TipAdvance::Blocks`).
    /// Deduplicates across tip regressions and reconnects and converts a
    /// changed node-disclosed hash into a reorg.
    pub(crate) fn record_reported(&mut self, height: u64, hash: Option<&str>) -> BlockRecord {
        self.record(height, hash, true)
    }

    /// Register a block fetched for any other reason (strip backfill). Never
    /// produces a `New` announcement — history stays history — but a changed
    /// hash at an already-known height still surfaces as a reorg.
    pub(crate) fn observe_fetched(&mut self, height: u64, hash: Option<&str>) -> BlockRecord {
        self.record(height, hash, false)
    }

    fn record(&mut self, height: u64, hash: Option<&str>, announce_new: bool) -> BlockRecord {
        let hash = hash
            .map(str::trim)
            .filter(|hash| !hash.is_empty() && hash.len() <= MAX_HASH_CHARS)
            .map(str::to_owned);
        let record = match self.reported.get_mut(&height) {
            Some(existing) => match (existing.as_deref(), hash.as_deref()) {
                (Some(previous), Some(new)) if previous != new => {
                    let previous = previous.to_owned();
                    *existing = hash;
                    BlockRecord::Reorg {
                        previous_hash: previous,
                    }
                }
                (None, Some(_)) => {
                    // First time the node disclosed this block's hash: learn
                    // it silently for future reorg detection.
                    *existing = hash;
                    BlockRecord::AlreadyReported
                }
                _ => BlockRecord::AlreadyReported,
            },
            None => {
                self.reported.insert(height, hash);
                if announce_new {
                    BlockRecord::New
                } else {
                    BlockRecord::AlreadyReported
                }
            }
        };
        self.prune();
        record
    }

    fn prune(&mut self) {
        while self.reported.len() > MAX_REPORTED_TRACKED {
            let oldest = *self.reported.keys().next().expect("non-empty reported map");
            self.reported.remove(&oldest);
        }
    }
}

/// What is known about one confirmed block, fetched via `sov_getBlockByHeight`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockInfo {
    pub(crate) hash: Option<String>,
    pub(crate) tx_count: Option<u64>,
    /// Miner-declared header timestamp in ms; feeds the block-time graph.
    pub(crate) timestamp_ms: Option<u64>,
}

/// One rendered tile of the confirmed strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tile {
    pub(crate) height: u64,
    pub(crate) tx_count: Option<u64>,
    pub(crate) timestamp_ms: Option<u64>,
    pub(crate) mine: bool,
}

/// Bounded cache of the most recent confirmed blocks. Confirmed blocks are
/// immutable, so a cached entry is refetched only when a reorg replaces it
/// (detected by a changed hash at the same height, which also invalidates
/// every cached descendant).
#[derive(Debug, Default)]
pub(crate) struct RecentBlocks {
    blocks: BTreeMap<u64, BlockInfo>,
}

impl RecentBlocks {
    fn window_start(tip: u64) -> u64 {
        tip.saturating_sub(RECENT_DEPTH - 1)
    }

    /// Heights in the visible window still missing from the cache, newest
    /// first, bounded to `MAX_FETCH_PER_REFRESH` per template refresh so a
    /// cold start backfills across cycles instead of stalling one.
    pub(crate) fn refresh_targets(&self, tip: u64) -> Vec<u64> {
        let mut targets = Vec::new();
        let mut height = tip;
        loop {
            if !self.blocks.contains_key(&height) {
                targets.push(height);
                if targets.len() >= MAX_FETCH_PER_REFRESH {
                    break;
                }
            }
            if height == Self::window_start(tip) {
                break;
            }
            height -= 1;
        }
        targets
    }

    /// Record a fetched block and evict everything that can no longer be
    /// trusted or shown: entries outside the window, and — when the hash at
    /// this height changed (reorg) — every cached block above it, which was
    /// built on the replaced block.
    pub(crate) fn insert(&mut self, tip: u64, height: u64, info: BlockInfo) {
        let reorged = self.blocks.get(&height).is_some_and(|existing| {
            existing.hash.is_some() && info.hash.is_some() && existing.hash != info.hash
        });
        if reorged {
            self.blocks.retain(|&cached, _| cached <= height);
        }
        self.blocks.insert(height, info);
        let start = Self::window_start(tip);
        self.blocks
            .retain(|&cached, _| cached >= start && cached <= tip);
    }

    /// The full strip, newest first: one tile per height in the window. A
    /// height whose fetch failed (or has not happened yet) still gets a tile,
    /// with an unknown transaction count for the placeholder rendering.
    pub(crate) fn tiles(&self, tip: u64, sealed: &SealedBlocks) -> Vec<Tile> {
        let mut tiles = Vec::new();
        let mut height = tip;
        loop {
            let info = self.blocks.get(&height);
            tiles.push(Tile {
                height,
                tx_count: info.and_then(|info| info.tx_count),
                timestamp_ms: info.and_then(|info| info.timestamp_ms),
                mine: sealed.sealed(height, info.and_then(|info| info.hash.as_deref())),
            });
            if height == Self::window_start(tip) {
                break;
            }
            height -= 1;
        }
        tiles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tx_count_reads_the_live_transactions_shape_or_digest_txids() {
        // Live mainnet `sov_getBlockByHeight`: {"header": {...},
        // "transactions": [...]} — the count is the array length.
        assert_eq!(
            tx_count(&json!({
                "header": {"height": 7, "txRoot": "22".repeat(32)},
                "transactions": [{"kind": "transfer"}, {"kind": "transfer"}, {"kind": "tip"}],
            })),
            Some(3)
        );
        assert_eq!(tx_count(&json!({"transactions": []})), Some(0));
        // `sov_getBlockDigest` equivalent: a txIds array.
        assert_eq!(tx_count(&json!({"txIds": ["aa", "bb"]})), Some(2));
        assert_eq!(
            tx_count(&json!({"block": {"height": 7, "transactions": [{}]}})),
            Some(1)
        );
        // Unknown stays unknown: no invented count. The pre-upgrade block
        // template carries only txRoot — no list, no count — and yields None.
        assert_eq!(
            tx_count(&json!({"height": 7, "txRoot": "22".repeat(32)})),
            None
        );
        assert_eq!(tx_count(&json!({"transactions": "three"})), None);
        // The upgraded node's template discloses an explicit integer count.
        assert_eq!(tx_count(&json!({"txCount": 12})), Some(12));
        assert_eq!(tx_count(&json!({"txCount": "12"})), None);
        // A disclosed txIds list is authoritative over the count field.
        assert_eq!(
            tx_count(&json!({"txCount": 12, "txIds": ["aa", "bb"]})),
            Some(2)
        );
    }

    #[test]
    fn block_hash_is_bounded_and_optional() {
        assert_eq!(
            block_hash(&json!({"hash": "ab".repeat(32)})),
            Some("ab".repeat(32))
        );
        assert_eq!(
            block_hash(&json!({"block": {"hash": "cd"}})),
            Some("cd".into())
        );
        assert_eq!(
            block_hash(&json!({"header": {"hash": "ee".repeat(32)}})),
            Some("ee".repeat(32))
        );
        assert_eq!(block_hash(&json!({"hash": ""})), None);
        assert_eq!(block_hash(&json!({"hash": "ff".repeat(65)})), None);
        // The live shape discloses no hash: fall back to None honestly.
        assert_eq!(
            block_hash(&json!({"header": {"height": 3}, "transactions": []})),
            None
        );
        assert_eq!(block_hash(&json!({"hash": 42})), None);
    }

    #[test]
    fn fee_and_mempool_parsers_match_the_live_node_shapes() {
        // Live mainnet `sov_estimateFee`: feeGrains is a decimal string.
        assert_eq!(
            fee_grains(&json!({
                "feeGrains": "1651760",
                "gasPriceGrains": "10",
                "gasUsed": 165_176,
                "kind": "transfer",
            })),
            Some(1_651_760)
        );
        assert_eq!(fee_grains(&json!({"feeGrains": 250})), Some(250));
        assert_eq!(fee_grains(&json!(250)), Some(250));
        assert_eq!(fee_grains(&json!({"feeGrains": "abc"})), None);
        assert_eq!(fee_grains(&json!({"feeGrains": "-5"})), None);
        assert_eq!(fee_grains(&json!({"minTip": 1_000})), None);
        assert_eq!(fee_grains(&json!("250")), None);

        // Live mainnet `sov_getMempoolSize`: a bare integer.
        assert_eq!(mempool_size(&json!(0)), Some(0));
        assert_eq!(mempool_size(&json!(17)), Some(17));
        assert_eq!(mempool_size(&json!({"size": 4})), Some(4));
        assert_eq!(mempool_size(&json!({"txs": 9})), None);
        assert_eq!(mempool_size(&json!(null)), None);
    }

    #[test]
    fn fee_floor_prefers_the_auction_floor_and_falls_back_to_fee_grains() {
        // Upgraded node: floorGrains (the live next-block auction floor) wins
        // over the flat feeGrains estimate.
        assert_eq!(
            fee_floor_grains(&json!({
                "feeGrains": "1651760",
                "floorGrains": "7000",
                "tipFloorGrains": "0",
                "gasPriceGrains": "10",
                "gasUsed": 165_176,
                "kind": "transfer",
            })),
            Some(7_000)
        );
        assert_eq!(
            fee_floor_grains(&json!({"floorGrains": 0, "feeGrains": "1651760"})),
            Some(0)
        );
        // Pre-upgrade node: no floorGrains — the flat estimate is used.
        assert_eq!(
            fee_floor_grains(&json!({"feeGrains": "1651760", "gasUsed": 165_176})),
            Some(1_651_760)
        );
        // A malformed floor falls back to the flat estimate, never a guess.
        assert_eq!(
            fee_floor_grains(&json!({"floorGrains": "abc", "feeGrains": 250})),
            Some(250)
        );
        assert_eq!(fee_floor_grains(&json!({"kind": "transfer"})), None);
    }

    #[test]
    fn mempool_histogram_parses_the_live_shape_and_rejects_malformed_replies() {
        // The exact upgraded-node reply shape (grains fields are strings).
        let parsed = mempool_histogram(&json!({
            "txCount": 2,
            "floorGrains": "7000",
            "poolFloorGrains": "0",
            "maxBlockTxs": 1,
            "buckets": [
                {"feeRateGrains": "7000", "txCount": 1, "totalBytes": 305},
                {"feeRateGrains": "0", "txCount": 1, "totalBytes": 262},
            ],
        }))
        .expect("live histogram shape parses");
        assert_eq!(parsed.tx_count, 2);
        assert_eq!(parsed.max_block_txs, 1);
        assert_eq!(
            parsed.buckets,
            vec![
                HistogramBucket {
                    fee_rate_grains: 7_000,
                    tx_count: 1,
                },
                HistogramBucket {
                    fee_rate_grains: 0,
                    tx_count: 1,
                },
            ]
        );

        // Old node / error / malformed: honest None, never invented buckets.
        assert_eq!(mempool_histogram(&json!(null)), None);
        assert_eq!(mempool_histogram(&json!({})), None);
        assert_eq!(
            mempool_histogram(&json!({"txCount": 2, "maxBlockTxs": 1})),
            None
        );
        assert_eq!(
            mempool_histogram(&json!({
                "txCount": 2,
                "maxBlockTxs": 1,
                "buckets": [{"feeRateGrains": "abc", "txCount": 1}],
            })),
            None
        );
        assert_eq!(
            mempool_histogram(&json!({
                "txCount": 2,
                "maxBlockTxs": 1,
                "buckets": "none",
            })),
            None
        );
    }

    fn histogram(max_block_txs: u64, buckets: &[(u64, u64)]) -> MempoolHistogram {
        MempoolHistogram {
            tx_count: buckets.iter().map(|(_, count)| count).sum(),
            max_block_txs,
            buckets: buckets
                .iter()
                .map(|&(fee_rate_grains, tx_count)| HistogramBucket {
                    fee_rate_grains,
                    tx_count,
                })
                .collect(),
        }
    }

    #[test]
    fn projected_blocks_pack_beyond_the_forming_block_by_node_capacity() {
        // The live two-transaction sample: capacity 1, so the 7000-grain tx
        // fills the forming block and the zero-tip tx is one projected block.
        assert_eq!(
            projected_blocks(&histogram(1, &[(7_000, 1), (0, 1)])),
            vec![ProjectedBlock {
                tx_count: 1,
                fee_min_grains: 0,
                fee_max_grains: 0,
            }]
        );

        // A bucket spanning blocks keeps its fee rate in each block's range.
        assert_eq!(
            projected_blocks(&histogram(3, &[(9_000, 2), (5_000, 4), (100, 1)])),
            vec![
                ProjectedBlock {
                    tx_count: 3,
                    fee_min_grains: 5_000,
                    fee_max_grains: 5_000,
                },
                ProjectedBlock {
                    tx_count: 1,
                    fee_min_grains: 100,
                    fee_max_grains: 100,
                },
            ]
        );

        // Everything fitting in the forming block projects nothing.
        assert_eq!(projected_blocks(&histogram(8, &[(7_000, 2)])), Vec::new());
        // An empty mempool or a zero capacity projects nothing.
        assert_eq!(projected_blocks(&histogram(4, &[])), Vec::new());
        assert_eq!(projected_blocks(&histogram(0, &[(7_000, 5)])), Vec::new());
    }

    #[test]
    fn projected_blocks_merge_the_tail_into_the_last_tile_at_the_cap() {
        // 1 forming + far more than the cap: capacity 2, 30 transactions in
        // one 400-grain bucket then 4 in a 10-grain bucket.
        let projected = projected_blocks(&histogram(2, &[(400, 30), (10, 4)]));
        assert_eq!(projected.len(), MAX_PROJECTED_BLOCKS);
        // 2 txs formed; the first cap-1 tiles carry 2 each; the last tile
        // absorbs the whole remaining tail including the low-tip bucket.
        for block in &projected[..MAX_PROJECTED_BLOCKS - 1] {
            assert_eq!(block.tx_count, 2);
            assert_eq!(block.fee_min_grains, 400);
            assert_eq!(block.fee_max_grains, 400);
        }
        let last = projected.last().expect("capped tail tile");
        let packed: u64 = projected.iter().map(|block| block.tx_count).sum();
        assert_eq!(packed, 34 - 2, "no pending transaction is dropped");
        assert_eq!(last.fee_min_grains, 10);
        assert_eq!(last.fee_max_grains, 400);
    }

    #[test]
    fn sealed_blocks_prefer_hash_identity_over_height() {
        let mut sealed = SealedBlocks::default();
        sealed.record(100, "aa".repeat(32));

        // Hash disclosed: only the exact sealed hash matches.
        assert!(sealed.sealed(100, Some(&"aa".repeat(32))));
        assert!(sealed.sealed(101, Some(&"aa".repeat(32))));
        // A same-height reorg replacement is NOT ours.
        assert!(!sealed.sealed(100, Some(&"bb".repeat(32))));

        // Hash not disclosed by the node: fall back to the accepted height.
        assert!(sealed.sealed(100, None));
        assert!(!sealed.sealed(99, None));
    }

    #[test]
    fn sealed_blocks_history_is_bounded() {
        let mut sealed = SealedBlocks::default();
        for height in 0..(MAX_SEALED_TRACKED as u64 + 8) {
            sealed.record(height, format!("{height:064x}"));
        }
        assert!(!sealed.sealed(0, None), "oldest entries must be evicted");
        assert!(sealed.sealed(MAX_SEALED_TRACKED as u64 + 7, None));
    }

    fn info(hash: &str, txs: u64) -> BlockInfo {
        BlockInfo {
            hash: Some(hash.to_owned()),
            tx_count: Some(txs),
            timestamp_ms: None,
        }
    }

    #[test]
    fn block_timestamp_reads_only_node_disclosed_millisecond_fields() {
        // Live-verified mainnet shape: header.timestamp_ms in milliseconds.
        assert_eq!(
            block_timestamp_ms(&json!({
                "header": {"height": 7, "timestamp_ms": 1_784_936_544_968_u64},
                "transactions": [],
            })),
            Some(1_784_936_544_968)
        );
        // The node's other (template) spelling is accepted equivalently.
        assert_eq!(
            block_timestamp_ms(&json!({"header": {"timestampMs": 5}})),
            Some(5)
        );
        assert_eq!(block_timestamp_ms(&json!({"timestamp_ms": 9})), Some(9));
        assert_eq!(
            block_timestamp_ms(&json!({"block": {"header": {"timestamp_ms": 3}}})),
            Some(3)
        );
        // Undisclosed or malformed stays unknown — never a guess.
        assert_eq!(block_timestamp_ms(&json!({"header": {"height": 7}})), None);
        assert_eq!(
            block_timestamp_ms(&json!({"header": {"timestamp_ms": "soon"}})),
            None
        );
        assert_eq!(block_timestamp_ms(&json!(null)), None);
    }

    #[test]
    fn tiles_carry_node_disclosed_timestamps_for_the_block_time_graph() {
        let mut recent = RecentBlocks::default();
        recent.insert(
            50,
            50,
            BlockInfo {
                hash: Some("aa".into()),
                tx_count: Some(1),
                timestamp_ms: Some(1_000),
            },
        );
        recent.insert(50, 49, info("bb", 2)); // no timestamp disclosed
        let tiles = recent.tiles(50, &SealedBlocks::default());
        assert_eq!(tiles[0].timestamp_ms, Some(1_000));
        assert_eq!(tiles[1].timestamp_ms, None, "unknown stays unknown");
    }

    #[test]
    fn refresh_targets_are_newest_first_and_bounded() {
        let mut recent = RecentBlocks::default();
        assert_eq!(recent.refresh_targets(50), vec![50, 49, 48, 47]);
        assert_eq!(
            recent.refresh_targets(2),
            vec![2, 1, 0],
            "a young chain never requests negative heights"
        );

        recent.insert(50, 50, info("aa", 1));
        recent.insert(50, 48, info("bb", 2));
        assert_eq!(recent.refresh_targets(50), vec![49, 47, 46, 45]);
    }

    #[test]
    fn tiles_cover_the_full_window_with_placeholders_and_highlight() {
        let mut recent = RecentBlocks::default();
        let mut sealed = SealedBlocks::default();
        sealed.record(49, "cc".repeat(32));

        recent.insert(50, 50, info("aa", 5));
        recent.insert(50, 49, info(&"cc".repeat(32), 2));
        let tiles = recent.tiles(50, &sealed);
        assert_eq!(tiles.len() as u64, RECENT_DEPTH);
        assert_eq!(
            tiles[0],
            Tile {
                height: 50,
                tx_count: Some(5),
                timestamp_ms: None,
                mine: false
            }
        );
        assert_eq!(
            tiles[1],
            Tile {
                height: 49,
                tx_count: Some(2),
                timestamp_ms: None,
                mine: true
            }
        );
        // Unfetched heights render as real heights with unknown tx counts.
        assert_eq!(
            tiles[2],
            Tile {
                height: 48,
                tx_count: None,
                timestamp_ms: None,
                mine: false
            }
        );
    }

    #[test]
    fn reorg_at_a_height_invalidates_every_cached_descendant() {
        let mut recent = RecentBlocks::default();
        recent.insert(50, 48, info("aa", 1));
        recent.insert(50, 49, info("bb", 2));
        recent.insert(50, 50, info("cc", 3));

        // Same height, different hash: 48 is replaced; 49 and 50 were built on
        // the old 48 and must be refetched instead of shown stale.
        recent.insert(50, 48, info("dd", 4));
        assert_eq!(recent.refresh_targets(50), vec![50, 49, 47, 46]);
        let tiles = recent.tiles(50, &SealedBlocks::default());
        assert_eq!(tiles[0].tx_count, None);
        assert_eq!(tiles[1].tx_count, None);
        assert_eq!(tiles[2].tx_count, Some(4));
    }

    #[test]
    fn tip_watch_baselines_once_and_never_replays_history() {
        let mut watch = TipWatch::default();
        watch.begin_session();
        // First successful poll: one baseline line, no block-found flood even
        // though the chain has 11_519 blocks of history.
        assert_eq!(
            watch.observe_tip(11_519),
            TipAdvance::Baseline { height: 11_519 }
        );
        // The same tip on the next poll announces nothing.
        assert_eq!(watch.observe_tip(11_519), TipAdvance::Unchanged);
        // A single new block is reported exactly once.
        assert_eq!(
            watch.observe_tip(11_520),
            TipAdvance::Blocks {
                report: vec![11_520],
                omitted: 0,
                omitted_from: 11_520,
                omitted_to: 11_519,
            }
        );
        assert_eq!(watch.observe_tip(11_520), TipAdvance::Unchanged);
    }

    #[test]
    fn tip_watch_rebaselines_on_reconnect_without_rereporting() {
        let mut watch = TipWatch::default();
        watch.begin_session();
        assert_eq!(watch.observe_tip(100), TipAdvance::Baseline { height: 100 });
        // The baseline height itself is covered by the baseline line and can
        // never be re-announced as a found block.
        assert_eq!(
            watch.record_reported(100, Some("99")),
            BlockRecord::AlreadyReported
        );
        assert_eq!(
            watch.observe_tip(101),
            TipAdvance::Blocks {
                report: vec![101],
                omitted: 0,
                omitted_from: 101,
                omitted_to: 100,
            }
        );
        assert_eq!(watch.record_reported(101, Some("aa")), BlockRecord::New);

        // The session drops; blocks 102..=105 appear while disconnected.
        // Reconnect: a single new baseline, never a four-line replay.
        watch.begin_session();
        assert_eq!(watch.observe_tip(105), TipAdvance::Baseline { height: 105 });
        // And a height+hash reported before the reconnect stays reported.
        assert_eq!(
            watch.record_reported(101, Some("aa")),
            BlockRecord::AlreadyReported
        );
    }

    #[test]
    fn tip_watch_bounds_gap_reporting_and_counts_the_omitted_tail() {
        let mut watch = TipWatch::default();
        watch.begin_session();
        assert_eq!(watch.observe_tip(50), TipAdvance::Baseline { height: 50 });

        // A jump within the bound reports every intervening height.
        assert_eq!(
            watch.observe_tip(53),
            TipAdvance::Blocks {
                report: vec![51, 52, 53],
                omitted: 0,
                omitted_from: 51,
                omitted_to: 50,
            }
        );

        // A jump beyond the bound reports only the newest MAX_GAP_BLOCKS
        // heights and says explicitly how many older ones were skipped —
        // bounded lines AND bounded follow-up RPCs, never silent truncation.
        assert_eq!(
            watch.observe_tip(53 + MAX_GAP_BLOCKS + 7),
            TipAdvance::Blocks {
                report: (61..=64).collect(),
                omitted: 7,
                omitted_from: 54,
                omitted_to: 60,
            }
        );
    }

    #[test]
    fn tip_watch_reports_a_reorg_distinctly_and_never_a_duplicate_block() {
        let mut watch = TipWatch::default();
        watch.begin_session();
        assert_eq!(watch.observe_tip(10), TipAdvance::Baseline { height: 10 });
        assert_eq!(
            watch.observe_tip(11),
            TipAdvance::Blocks {
                report: vec![11],
                omitted: 0,
                omitted_from: 11,
                omitted_to: 10,
            }
        );
        assert_eq!(watch.record_reported(11, Some("aa")), BlockRecord::New);

        // Idempotence: the same height+hash is never reported twice.
        assert_eq!(
            watch.record_reported(11, Some("aa")),
            BlockRecord::AlreadyReported
        );

        // The tip walking back to 10 announces nothing by itself…
        assert_eq!(watch.observe_tip(10), TipAdvance::Unchanged);
        // …and height 11 reappearing with a DIFFERENT hash is a reorg line,
        // not a second block-found line.
        assert_eq!(
            watch.observe_tip(11),
            TipAdvance::Blocks {
                report: vec![11],
                omitted: 0,
                omitted_from: 11,
                omitted_to: 10,
            }
        );
        assert_eq!(
            watch.record_reported(11, Some("bb")),
            BlockRecord::Reorg {
                previous_hash: "aa".into(),
            }
        );
        // The replacement is now the known block at 11.
        assert_eq!(
            watch.record_reported(11, Some("bb")),
            BlockRecord::AlreadyReported
        );
    }

    #[test]
    fn tip_watch_backfill_learns_hashes_silently_and_flags_reorgs() {
        let mut watch = TipWatch::default();
        watch.begin_session();
        assert_eq!(watch.observe_tip(20), TipAdvance::Baseline { height: 20 });

        // Strip backfill of history (heights below the baseline) must never
        // produce a block-found announcement…
        assert_eq!(
            watch.observe_fetched(15, Some("cc")),
            BlockRecord::AlreadyReported
        );
        // …but it registers the hash, so a later replacement IS a reorg.
        assert_eq!(
            watch.observe_fetched(15, Some("dd")),
            BlockRecord::Reorg {
                previous_hash: "cc".into(),
            }
        );

        // The baseline block itself: hash learned silently on first fetch,
        // reorg on replacement.
        assert_eq!(
            watch.observe_fetched(20, Some("ee")),
            BlockRecord::AlreadyReported
        );
        assert_eq!(
            watch.observe_fetched(20, Some("ff")),
            BlockRecord::Reorg {
                previous_hash: "ee".into(),
            }
        );

        // A node that discloses no hash (the live pre-upgrade shape) can
        // never produce a false reorg.
        assert_eq!(
            watch.observe_fetched(20, None),
            BlockRecord::AlreadyReported
        );
    }

    #[test]
    fn tip_watch_memory_is_bounded_and_rejects_oversized_hashes() {
        let mut watch = TipWatch::default();
        watch.begin_session();
        assert_eq!(watch.observe_tip(0), TipAdvance::Baseline { height: 0 });
        for height in 1..=(MAX_REPORTED_TRACKED as u64 + 16) {
            watch.observe_tip(height);
            watch.record_reported(height, Some(&format!("{height:064x}")));
        }
        assert!(watch.reported.len() <= MAX_REPORTED_TRACKED);
        // Pruned heights fell off the oldest end; the newest stay known.
        assert_eq!(
            watch.record_reported(
                MAX_REPORTED_TRACKED as u64 + 16,
                Some(&format!("{:064x}", MAX_REPORTED_TRACKED as u64 + 16)),
            ),
            BlockRecord::AlreadyReported
        );

        // A node-supplied hash beyond the sanity bound is ignored, exactly
        // like `block_hash`, so it cannot grow memory or fake a reorg.
        let oversized = "ff".repeat(MAX_HASH_CHARS);
        assert_eq!(
            watch.record_reported(MAX_REPORTED_TRACKED as u64 + 16, Some(&oversized)),
            BlockRecord::AlreadyReported
        );
    }

    #[test]
    fn cache_prunes_to_the_visible_window() {
        let mut recent = RecentBlocks::default();
        recent.insert(100, 100, info("aa", 1));
        recent.insert(100, 100 - (RECENT_DEPTH - 1), info("bb", 2));
        // The tip advancing pushes both old entries out of the window.
        let far_tip = 100 + RECENT_DEPTH;
        recent.insert(far_tip, far_tip, info("cc", 3));
        let window_start = far_tip - (RECENT_DEPTH - 1);
        assert!(recent
            .refresh_targets(far_tip)
            .iter()
            .all(|height| *height >= window_start));
        assert_eq!(
            recent.tiles(far_tip, &SealedBlocks::default())[0].tx_count,
            Some(3)
        );
    }
}
