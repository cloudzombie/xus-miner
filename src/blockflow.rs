//! Read-only "block flow" telemetry: a mempool.space-style strip of recent
//! confirmed blocks, the forming template block, and the fee-to-get-in
//! indicator, derived exclusively from real SOV node RPC responses
//! (`sov_getBlockByHeight`, `sov_getBlockTemplate`, `sov_getMempoolSize`,
//! `sov_estimateFee`). A datum a node does not supply stays `None` and is
//! rendered as a neutral placeholder; nothing here invents, estimates, or
//! fabricates a value.
//!
//! OUT OF SCOPE (documented seam): mempool.space renders several *projected*
//! pending blocks, each a distinct fee-rate bucket. That requires the
//! mempool's fee histogram, which the SOV node does not expose today —
//! `sov_getMempoolSize` returns one count and `sov_estimateFee` one estimate.
//! Exposing a histogram is an additive node RPC and therefore a separately
//! authorized SOV-repository task; this module must not fake the buckets.
//! When such an RPC exists, extend the engine's `block_flow` event with the
//! histogram and add the projected tiles beside the single forming tile.

use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::VecDeque;

/// Confirmed-block tiles kept in the strip (newest first).
pub(crate) const RECENT_DEPTH: u64 = 8;
/// Upper bound on `sov_getBlockByHeight` calls per template refresh.
pub(crate) const MAX_FETCH_PER_REFRESH: usize = 4;
/// Accepted block submissions remembered for the "yours" highlight.
const MAX_SEALED_TRACKED: usize = 32;
/// Sanity bound for a hash string taken from an RPC response.
const MAX_HASH_CHARS: usize = 128;

/// The object that actually carries block fields: some RPC shapes nest the
/// block under a `block` key; use it when present, else the value itself.
fn block_facet(value: &Value) -> &Value {
    match value.get("block") {
        Some(inner) if inner.is_object() => inner,
        _ => value,
    }
}

/// Real transaction count of a block or template response: the length of its
/// `txIds` array, or an explicit integer `txCount`. Absent both, the count is
/// unknown and the UI must show a placeholder.
pub(crate) fn tx_count(value: &Value) -> Option<u64> {
    let facet = block_facet(value);
    if let Some(ids) = facet.get("txIds").and_then(Value::as_array) {
        return Some(ids.len() as u64);
    }
    facet.get("txCount").and_then(Value::as_u64)
}

/// The block's own hash as disclosed by the node, if any.
pub(crate) fn block_hash(value: &Value) -> Option<String> {
    block_facet(value)
        .get("hash")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|hash| !hash.is_empty() && hash.len() <= MAX_HASH_CHARS)
        .map(str::to_owned)
}

/// A single fee-for-inclusion estimate from `sov_estimateFee`. The reply may
/// be a bare integer or an object; only recognized numeric fields are
/// accepted, and an unrecognized shape yields `None` (placeholder), never a
/// guess.
pub(crate) fn fee_estimate(value: &Value) -> Option<u64> {
    if let Some(fee) = value.as_u64() {
        return Some(fee);
    }
    ["minTip", "tipFloor", "feeFloor", "fee", "estimate"]
        .iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64))
}

/// The pending-transaction count from `sov_getMempoolSize`: a bare integer or
/// a recognized numeric field of an object reply.
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

/// What is known about one confirmed block, fetched via `sov_getBlockByHeight`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockInfo {
    pub(crate) hash: Option<String>,
    pub(crate) tx_count: Option<u64>,
}

/// One rendered tile of the confirmed strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tile {
    pub(crate) height: u64,
    pub(crate) tx_count: Option<u64>,
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
    fn tx_count_comes_only_from_real_txids_or_txcount() {
        assert_eq!(
            tx_count(&json!({"height": 7, "txIds": ["aa", "bb", "cc"]})),
            Some(3)
        );
        assert_eq!(tx_count(&json!({"txIds": []})), Some(0));
        assert_eq!(
            tx_count(&json!({"block": {"height": 7, "txIds": ["aa"]}})),
            Some(1)
        );
        assert_eq!(tx_count(&json!({"txCount": 12})), Some(12));
        // Unknown stays unknown: no invented count.
        assert_eq!(tx_count(&json!({"height": 7})), None);
        assert_eq!(tx_count(&json!({"txIds": "aa,bb"})), None);
        assert_eq!(tx_count(&json!({"txCount": -1})), None);
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
        assert_eq!(block_hash(&json!({"hash": ""})), None);
        assert_eq!(block_hash(&json!({"hash": "ff".repeat(65)})), None);
        assert_eq!(block_hash(&json!({"height": 3})), None);
        assert_eq!(block_hash(&json!({"hash": 42})), None);
    }

    #[test]
    fn fee_and_mempool_parsers_accept_numbers_and_known_fields_only() {
        assert_eq!(fee_estimate(&json!(250)), Some(250));
        assert_eq!(fee_estimate(&json!({"minTip": 1_000})), Some(1_000));
        assert_eq!(fee_estimate(&json!({"feeFloor": 3})), Some(3));
        assert_eq!(fee_estimate(&json!({"surprise": 9})), None);
        assert_eq!(fee_estimate(&json!("250")), None);
        assert_eq!(fee_estimate(&json!(-1)), None);

        assert_eq!(mempool_size(&json!(17)), Some(17));
        assert_eq!(mempool_size(&json!({"size": 4})), Some(4));
        assert_eq!(mempool_size(&json!({"pending": 0})), Some(0));
        assert_eq!(mempool_size(&json!({"txs": 9})), None);
        assert_eq!(mempool_size(&json!(null)), None);
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
        }
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
                mine: false
            }
        );
        assert_eq!(
            tiles[1],
            Tile {
                height: 49,
                tx_count: Some(2),
                mine: true
            }
        );
        // Unfetched heights render as real heights with unknown tx counts.
        assert_eq!(
            tiles[2],
            Tile {
                height: 48,
                tx_count: None,
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
    fn cache_prunes_to_the_visible_window() {
        let mut recent = RecentBlocks::default();
        recent.insert(50, 50, info("aa", 1));
        recent.insert(50, 43, info("bb", 2));
        // The tip advancing pushes 43 out of the window.
        recent.insert(58, 58, info("cc", 3));
        assert!(recent
            .refresh_targets(58)
            .iter()
            .all(|height| *height >= 51));
        assert_eq!(
            recent.tiles(58, &SealedBlocks::default())[0].tx_count,
            Some(3)
        );
    }
}
