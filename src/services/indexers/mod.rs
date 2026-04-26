//! Torznab/newznab indexer abstraction (issue #28 PR A).
//!
//! ## Scope of this PR
//!
//! Foundation only — the [`Indexer`] trait, the [`Release`] /
//! [`SearchQuery`] / [`IndexerCaps`] data model, and pure-function
//! helpers for the auto-search dedup pass and concurrent fan-out.
//! No concrete [`Indexer`] impls live here yet; the
//! `TorznabIndexer` impl lands in PR B alongside the search-pipeline
//! integration that actually populates `Vec<Arc<dyn Indexer>>` from
//! the [`crate::models::indexers`] table.
//!
//! ## Why Nyaa stays out-of-band
//!
//! Per plan decision #1, the existing direct Nyaa scraper in
//! [`crate::services::nyaa`] is NOT adapted to this trait. The
//! search pipeline dispatches to Nyaa-direct + fans out to
//! `Indexer` impls in parallel, then merges. Conforming Nyaa to
//! the trait would have meant adding [`Release`] fields like
//! `nyaa_description: Option<String>` that only one impl
//! populates — a noisy contract — and the source-classification
//! pipeline already reads Nyaa's description body directly. Pretending
//! the sources are uniform would have hidden that coupling.
//!
//! ## Protocol notes (from research, 2026-04-25)
//!
//! Authoritative shapes that any future impl must respect:
//!
//! - **URL shape is opaque to Ryokan.** Prowlarr emits
//!   `http://host:9696/{N}/api?apikey={KEY}&t=...`; Jackett emits
//!   `http://host:9117/api/v2.0/indexers/{slug}/results/torznab/api?apikey={KEY}&t=...`.
//!   Both end in `/api` and accept torznab params after `?`. The
//!   user pastes the full base URL verbatim from each tool's
//!   "Copy Torznab Url" button; Ryokan must not parse or
//!   reconstruct it.
//! - **Errors come back as HTTP 200 with `<error code="N"
//!   description="..."/>` bodies.** Real impls (Prowlarr, Jackett)
//!   also return non-200 in some paths (Prowlarr 401 on bad apikey
//!   before the torznab layer); both must be handled.
//! - **Anime category is `5070`** in the standard torznab namespace.
//!   AnimeTosho via Prowlarr historically mis-tagged anime as
//!   `5999` (Other) — title-parse fallback is required if the cat
//!   doesn't include 5070.
//! - **Per-indexer rate limits live inside Prowlarr/Jackett,** not
//!   the indexer itself. They surface as `429 Retry-After`. Mirror
//!   the cooldown pattern from [`crate::services::anilist::rate_limit`].
//! - **`tvsearch` with `cat=5070&q=<title>` is the right anime
//!   path.** `season`/`ep` params don't translate cleanly because
//!   anime trackers key on absolute episode numbers in titles.

pub mod torznab;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Standard torznab category id for anime. See doc-comment above for
/// why title-parse fallback is needed when this is absent from a
/// release's reported categories.
pub const TORZNAB_CAT_ANIME: i32 = 5070;

/// Default per-indexer search timeout when the row's
/// `request_timeout_secs` is NULL. Decision #7 — tighter than
/// Sonarr's 100s default because Ryokan's interactive search
/// surface needs lower user-perceived latency. Overridable
/// process-wide via `RYOKAN_INDEXER_DEFAULT_TIMEOUT_SECS`.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Indexer caps cache TTL. Decision #6 — matches Sonarr's
/// `NewznabCapabilitiesProvider.cs` 7-day default. The search
/// pipeline re-fetches lazily on next read past the TTL; manual
/// "Refresh caps" button on the indexer edit page covers the
/// out-of-band edit case.
pub const CAPS_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

/// What an [`Indexer::search`] caller asks for. Mirrors torznab's
/// `t=tvsearch` parameter set; the impl translates to the wire
/// format. `q` is the only free-text input — `season`/`ep` are
/// deliberately omitted because anime trackers key on absolute
/// episode numbers in release titles, not season+ep.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub q: String,
    /// Torznab category ids. Defaults to `[5070]` (anime) when
    /// empty. Multiple ids OR together on the wire (`cat=5070,5080`).
    pub categories: Vec<i32>,
    /// Page size. None lets the impl pick (typically the indexer's
    /// caps-reported default). Must be ≤ caps `max_limit`.
    pub limit: Option<u32>,
    /// 0-based offset for paging. None = 0.
    pub offset: Option<u32>,
}

/// One release row from a torznab response. Field set is the union
/// of what real Prowlarr/Jackett deployments emit; impl-specific
/// fields go in [`extra`] so the core type stays portable across
/// indexers.
///
/// Source classification consumes [`title`] + [`size_bytes`] +
/// [`info_hash`] (when present) — same inputs the existing
/// pipeline derives from a Nyaa scrape. The Nyaa-description-body
/// signal is unavailable here; classification degrades to four
/// layers (filename + ffprobe + temporal + group-map).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// FK to [`crate::models::indexers::Indexer::id`] of the
    /// indexer that surfaced this release. The dedup pass
    /// (decision #3) attributes each (infohash, indexer) pair to
    /// the lowest-priority-number indexer.
    pub indexer_id: i64,
    /// Snapshot of the indexer's priority at search time so a
    /// later DB edit can't change attribution retroactively.
    pub indexer_priority: i32,
    pub title: String,
    /// Stable per-release identifier from the torznab `<guid>`
    /// element. Used as a dedup key when [`info_hash`] is empty.
    pub guid: String,
    /// Download URL. For Prowlarr this is a proxy URL with the
    /// apikey appended; the .torrent fetch must go through Prowlarr.
    /// Per research note: stale on Prowlarr restart, so don't cache
    /// across days.
    pub link: String,
    /// Magnet URI when the indexer surfaces one, else empty.
    pub magnet: String,
    /// Unix timestamp of `<pubDate>`. 0 when missing/unparseable.
    pub publish_date: i64,
    pub size_bytes: u64,
    pub seeders: i32,
    pub leechers: i32,
    /// Lowercase hex; empty when the indexer doesn't expose it
    /// (some private trackers omit it). Dedup falls back to
    /// [`guid`] when this is empty.
    pub info_hash: String,
    /// Standard torznab category ids on this release. May contain
    /// indexer-specific subcategory ids beyond the well-known
    /// 5000-series. Empty is legal — the title-parse fallback
    /// catches anime mis-tags like AnimeTosho via Prowlarr's
    /// 5999 issue.
    pub categories: Vec<i32>,
    /// `1.0` = full count, `0.0` = freeleech. Some private
    /// trackers expose this; public trackers don't. None when
    /// the indexer doesn't emit the attr.
    pub download_volume_factor: Option<f32>,
    pub upload_volume_factor: Option<f32>,
    /// Catch-all for impl-specific torznab attrs not promoted to
    /// first-class fields. Inspector-friendly only — scoring path
    /// must not key off these.
    #[serde(default)]
    pub extra: HashMap<String, String>,
}

/// Caps response shape. Cached as JSON in `indexers.caps_json`
/// per the 7-day TTL. The settings UI renders [`categories`] as a
/// multi-select on the per-indexer config form.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexerCaps {
    pub categories: Vec<CategoryCap>,
    pub search_modes: Vec<SearchModeCap>,
    /// Server-reported maximum `limit` per request. None when the
    /// caps response doesn't carry it; defaults to spec value 100.
    pub max_limit: Option<u32>,
    /// Server-reported default `limit`. None when missing; spec
    /// default is 50.
    pub default_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryCap {
    pub id: i32,
    pub name: String,
    /// Subcategories nested under this top-level category id.
    /// Most indexers report a flat list; Prowlarr nests where the
    /// upstream tracker has subcats (e.g., 5070 Anime → 5080
    /// Anime/Movies on some private trackers).
    #[serde(default)]
    pub subcategories: Vec<CategoryCap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchModeCap {
    /// `"search"` / `"tvsearch"` / `"movie"` / `"music"` / `"book"`.
    pub mode: String,
    pub available: bool,
    /// Supported params (`q`, `cat`, `season`, `ep`, `tvdbid`,
    /// `imdbid`, etc.). Per-mode.
    #[serde(default)]
    pub supported_params: Vec<String>,
}

/// The torznab/newznab indexer trait. Every impl talks to a single
/// indexer instance (a row in the `indexers` table). The search
/// pipeline holds these as `Vec<Arc<dyn Indexer>>` so the fan-out
/// can run them concurrently.
///
/// `async_trait` (rather than the stable native syntax) for the
/// same reason [`crate::services::download_client::DownloadClient`]
/// uses it: object-safety + Send-bound futures on Tokio's
/// multi-threaded runtime require the boxed-future macro.
#[async_trait::async_trait]
pub trait Indexer: Send + Sync {
    /// FK to `indexers.id`.
    fn id(&self) -> i64;
    fn name(&self) -> &str;
    /// Sonarr-convention priority (lower = preferred). Drives
    /// auto-search dedup attribution per [`dedup_for_auto_search`]
    /// and the fan-out concurrency order.
    fn priority(&self) -> i32;
    fn is_private_tracker(&self) -> bool;

    /// Fetch capabilities from `t=caps`. Impls should respect the
    /// 7-day TTL on the row's [`caps_json`] cache; the search-path
    /// caller persists fresh JSON via
    /// [`crate::models::indexers::update_caps`] when this returns
    /// after a network round-trip.
    async fn caps(&self) -> Result<IndexerCaps, String>;

    /// Search this indexer for releases matching `query`. Impls
    /// are responsible for:
    ///
    /// - Parsing torznab `<error code="N"/>` bodies even on HTTP
    ///   200 (per protocol).
    /// - Honoring 429 + `Retry-After` from upstream.
    /// - Filtering results below the indexer's configured
    ///   `min_seeders` *before* return — the search pipeline's
    ///   scoring runs on whatever this returns, so a low-seeder
    ///   release leaking into the candidate set is wasted work.
    /// - Stamping each [`Release`] with `indexer_id` +
    ///   `indexer_priority` so the dedup pass can attribute
    ///   correctly.
    async fn search(&self, query: &SearchQuery) -> Result<Vec<Release>, String>;
}

/// Per-indexer search outcome. Surfaces partial failures so the
/// auto-search inspector can show "AnimeTosho: timeout after 30s"
/// alongside successful results from other indexers (plan
/// "scoring inspector changes"). PR A defines the type; PR B's
/// fan-out helper produces it.
#[derive(Debug)]
pub struct IndexerSearchOutcome {
    pub indexer_id: i64,
    pub indexer_name: String,
    pub result: Result<Vec<Release>, String>,
}

/// Auto-search dedup pass (decision #3). Collapses the same
/// (infohash, ?) release reported by multiple indexers into a
/// single [`Release`], attributing to the lowest-priority-number
/// indexer (Sonarr convention) and aggregating seeder counts via
/// `max` (most accurate signal across reporting indexers).
///
/// The dedup key is `info_hash` when present; otherwise `guid`.
/// The (lossy) fallback exists because some private trackers omit
/// infohash from torznab responses — without the guid fallback,
/// every release from those indexers would slip past dedup and
/// flood the candidate set.
///
/// **Interactive search uses a different policy:** no cross-indexer
/// dedup, one row per `(indexer, infohash)` pair so the user can
/// pick their preferred tracker. That helper is
/// [`merge_for_interactive_search`].
pub fn dedup_for_auto_search(releases: Vec<Release>) -> Vec<Release> {
    let mut by_key: HashMap<String, Release> = HashMap::new();
    for release in releases {
        let key = if !release.info_hash.is_empty() {
            release.info_hash.to_ascii_lowercase()
        } else if !release.guid.is_empty() {
            release.guid.clone()
        } else {
            // No infohash, no guid — keep the release but key by
            // a uniqueness-safe value so it doesn't collide with
            // other no-key releases. Using the title alone would
            // collide across indexers; including indexer_id keeps
            // them separate without losing them entirely.
            format!("__{}_{}", release.indexer_id, release.title)
        };
        match by_key.get_mut(&key) {
            None => {
                by_key.insert(key, release);
            }
            Some(existing) => {
                // Lower priority number = preferred indexer for
                // attribution. Tiebreak on indexer_id ascending so
                // the result is stable across calls.
                let take_new = release.indexer_priority < existing.indexer_priority
                    || (release.indexer_priority == existing.indexer_priority
                        && release.indexer_id < existing.indexer_id);
                let merged_seeders = existing.seeders.max(release.seeders);
                let merged_leechers = existing.leechers.max(release.leechers);
                if take_new {
                    *existing = release;
                }
                existing.seeders = merged_seeders;
                existing.leechers = merged_leechers;
            }
        }
    }
    let mut out: Vec<Release> = by_key.into_values().collect();
    // Stable sort by priority then id so callers downstream don't
    // see HashMap iteration nondeterminism.
    out.sort_by(|a, b| {
        a.indexer_priority
            .cmp(&b.indexer_priority)
            .then(a.indexer_id.cmp(&b.indexer_id))
    });
    out
}

/// Interactive-search merge: no cross-indexer dedup. Returns one
/// row per `(indexer_id, info_hash | guid)` pair so the user sees
/// per-tracker rows in the search-results table and can pick
/// based on tracker-specific factors (ratio rules, freeleech,
/// upload-goals). Sorted by priority then by published date
/// descending so newer releases bubble up within the same
/// indexer.
pub fn merge_for_interactive_search(releases: Vec<Release>) -> Vec<Release> {
    let mut by_key: HashMap<(i64, String), Release> = HashMap::new();
    for release in releases {
        let inner = if !release.info_hash.is_empty() {
            release.info_hash.to_ascii_lowercase()
        } else if !release.guid.is_empty() {
            release.guid.clone()
        } else {
            format!("__{}", release.title)
        };
        let key = (release.indexer_id, inner);
        // Same-key collisions inside one indexer are degenerate
        // (same release listed twice in one response) — keep the
        // first.
        by_key.entry(key).or_insert(release);
    }
    let mut out: Vec<Release> = by_key.into_values().collect();
    out.sort_by(|a, b| {
        a.indexer_priority
            .cmp(&b.indexer_priority)
            .then(b.publish_date.cmp(&a.publish_date))
    });
    out
}

/// Concurrent fan-out across configured indexers. Each indexer
/// runs in its own future with the indexer's own request timeout;
/// a slow indexer holds up only its own slot, not the whole
/// search. Failures are captured as [`IndexerSearchOutcome`]
/// items rather than propagated — the auto-search inspector
/// shows per-indexer success/failure instead of failing the
/// whole search when one indexer dies.
///
/// PR A wires the helper but no callers exist yet. PR B's
/// auto-search integration calls this after the Nyaa-direct
/// fetch and merges the [`Release`] vecs from successful
/// outcomes via [`dedup_for_auto_search`].
pub async fn fan_out_search(
    indexers: &[Arc<dyn Indexer>],
    query: &SearchQuery,
) -> Vec<IndexerSearchOutcome> {
    use futures_util::future::join_all;
    let futures = indexers.iter().map(|idx| async move {
        IndexerSearchOutcome {
            indexer_id: idx.id(),
            indexer_name: idx.name().to_string(),
            result: idx.search(query).await,
        }
    });
    join_all(futures).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(
        indexer_id: i64,
        priority: i32,
        info_hash: &str,
        guid: &str,
        title: &str,
        seeders: i32,
    ) -> Release {
        Release {
            indexer_id,
            indexer_priority: priority,
            title: title.to_string(),
            guid: guid.to_string(),
            link: String::new(),
            magnet: String::new(),
            publish_date: 0,
            size_bytes: 1_000_000_000,
            seeders,
            leechers: 0,
            info_hash: info_hash.to_string(),
            categories: vec![TORZNAB_CAT_ANIME],
            download_volume_factor: None,
            upload_volume_factor: None,
            extra: HashMap::new(),
        }
    }

    // ── dedup_for_auto_search ────────────────────────────────────────

    #[test]
    fn dedup_keeps_single_release_unchanged() {
        let input = vec![release(1, 25, "abc123", "g1", "Show", 10)];
        let out = dedup_for_auto_search(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].info_hash, "abc123");
        assert_eq!(out[0].seeders, 10);
    }

    #[test]
    fn dedup_attributes_to_lower_priority_indexer() {
        // Two indexers report the same infohash. The lower priority
        // number (Sonarr convention) wins attribution.
        let input = vec![
            release(2, 50, "abc123", "g1", "Show (mirror)", 5),
            release(1, 5, "abc123", "g2", "Show (preferred)", 8),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(out.len(), 1, "same infohash must collapse to one row");
        // Attribution: indexer 1 wins (priority 5 < 50).
        assert_eq!(out[0].indexer_id, 1);
        assert_eq!(out[0].title, "Show (preferred)");
    }

    #[test]
    fn dedup_aggregates_seeders_via_max_across_reporters() {
        // Same release, two reports — keep the higher seeder count
        // since indexers can disagree by minutes-old data and max
        // is most likely accurate.
        let input = vec![
            release(1, 5, "abc123", "g1", "Show", 8),
            release(2, 50, "abc123", "g2", "Show (mirror)", 42),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seeders, 42, "max seeders across reporters");
    }

    #[test]
    fn dedup_falls_back_to_guid_when_infohash_empty() {
        // Some private trackers omit infohash. Dedup must still
        // collapse same-guid rows or the same release from one
        // indexer would appear twice.
        let input = vec![
            release(1, 5, "", "private-guid-1", "Show", 5),
            release(1, 5, "", "private-guid-1", "Show", 5),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(
            out.len(),
            1,
            "same guid must collapse even without infohash"
        );
    }

    #[test]
    fn dedup_keeps_no_key_releases_separate_per_indexer() {
        // Pathological: no infohash, no guid. Don't collapse them
        // into one row across indexers (we can't tell if they're
        // the same release) but also don't lose them.
        let input = vec![
            release(1, 5, "", "", "Show A", 5),
            release(2, 50, "", "", "Show A", 5),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(
            out.len(),
            2,
            "no-key releases from different indexers stay distinct"
        );
    }

    #[test]
    fn dedup_output_is_sorted_by_priority_ascending() {
        // Stable order = deterministic UI rendering across calls.
        let input = vec![
            release(2, 50, "h2", "g2", "Low priority", 5),
            release(1, 5, "h1", "g1", "High priority", 5),
            release(3, 25, "h3", "g3", "Mid priority", 5),
        ];
        let out = dedup_for_auto_search(input);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].indexer_priority, 5);
        assert_eq!(out[1].indexer_priority, 25);
        assert_eq!(out[2].indexer_priority, 50);
    }

    // ── merge_for_interactive_search ────────────────────────────────

    #[test]
    fn interactive_keeps_one_row_per_indexer_per_release() {
        // Decision #3 — interactive search shows the user one row
        // per (indexer, infohash) so they can pick their preferred
        // tracker.
        let input = vec![
            release(1, 5, "abc123", "g1", "Show (Tracker A)", 10),
            release(2, 50, "abc123", "g2", "Show (Tracker B)", 8),
        ];
        let out = merge_for_interactive_search(input);
        assert_eq!(out.len(), 2, "both indexers' rows visible to user");
    }

    #[test]
    fn interactive_collapses_dup_within_same_indexer() {
        // A torznab response listing the same release twice (e.g.,
        // post + announce) should still show one row.
        let input = vec![
            release(1, 5, "abc123", "g1", "Show", 10),
            release(1, 5, "abc123", "g1", "Show", 10),
        ];
        let out = merge_for_interactive_search(input);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn interactive_sorts_by_priority_then_publish_date_desc() {
        let mut newer = release(1, 5, "h1", "g1", "Newer", 5);
        newer.publish_date = 1_000_000;
        let mut older = release(1, 5, "h2", "g2", "Older", 5);
        older.publish_date = 500_000;
        let other = release(2, 50, "h3", "g3", "Other indexer", 5);
        let input = vec![older, newer, other];
        let out = merge_for_interactive_search(input);
        assert_eq!(out.len(), 3);
        // Within indexer 1: newer first.
        assert_eq!(out[0].title, "Newer");
        assert_eq!(out[1].title, "Older");
        // Indexer 2 last (priority 50).
        assert_eq!(out[2].title, "Other indexer");
    }
}
