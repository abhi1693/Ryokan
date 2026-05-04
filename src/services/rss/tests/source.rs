use super::super::{RssItem, RssSource, resolve_dispatch_for_item, source_dedup_key};

#[test]
fn nyaa_source_labels_as_nyaa() {
    assert_eq!(RssSource::Nyaa.label(), "nyaa");
}

#[test]
fn user_feed_label_uses_feed_prefix() {
    let s = RssSource::UserFeed {
        id: 7,
        name: "SubsPlease 1080p".into(),
    };
    assert_eq!(s.label(), "feed:SubsPlease 1080p");
}

#[test]
fn indexer_label_uses_kind_prefix() {
    // Pin so log-grep `^torznab:` / `^newznab:` filters work
    // for filtering RSS decisions by indexer protocol.
    let t = RssSource::Indexer {
        id: 1,
        name: "Animebytes".into(),
        kind: "torznab".into(),
    };
    let n = RssSource::Indexer {
        id: 2,
        name: "NZBgeek".into(),
        kind: "newznab".into(),
    };
    assert_eq!(t.label(), "torznab:Animebytes");
    assert_eq!(n.label(), "newznab:NZBgeek");
}

// ─── Cross-source dedup-key + dispatch pinning ─────────────
//
// Mutation-testing audit (mutants.out.pre-pull) flagged
// `source_dedup_key` as the densest concentration of undetected
// mutants in the file: every return-value substitution survived,
// including the `("xyzzy", None)` garbage variant. That means no
// existing test exercised the function — RSS dedup, the integrity
// boundary that prevents a release from being grabbed twice across
// sources, was de facto unverified.
//
// `resolve_dispatch_for_item` had a similar shape — its whole-body
// `-> None` substitution survived. Pinning it requires an AppState
// fixture with a pool entry, but a single positive case kills the
// substitution. See mutants.out/PLAN.md Item 3.

#[test]
fn source_dedup_key_for_nyaa_returns_static_nyaa_with_no_id() {
    // Nyaa is the singleton out-of-band source — no row in `rss_feeds`
    // or `indexers`, so the second tuple element is intentionally None.
    // A mutation flipping the prefix to "" or "xyzzy" or the id from
    // None to Some(_) breaks this assertion.
    assert_eq!(source_dedup_key(&RssSource::Nyaa), ("nyaa", None));
}

#[test]
fn source_dedup_key_for_indexer_uses_indexer_prefix_and_carries_id() {
    // Pre-pull triage: the entire `Indexer { id, .. } => ("indexer",
    // Some(*id))` arm could be replaced with garbage and no test would
    // notice. Pin both halves: the static "indexer" prefix (so dedup
    // can't collide across source kinds) and the id round-trip (so
    // two different indexers stay distinct).
    let s = RssSource::Indexer {
        id: 5,
        name: "Animebytes".into(),
        kind: "torznab".into(),
    };
    assert_eq!(source_dedup_key(&s), ("indexer", Some(5)));
}

#[test]
fn source_dedup_key_for_userfeed_uses_direct_prefix_and_carries_id() {
    // Same shape as the Indexer arm but for UserFeed — distinct prefix
    // ("direct" not "indexer"), same id round-trip.
    let s = RssSource::UserFeed {
        id: 5,
        name: "SubsPlease 1080p".into(),
    };
    assert_eq!(source_dedup_key(&s), ("direct", Some(5)));
}

#[test]
fn source_dedup_key_distinguishes_indexer_from_userfeed_at_same_id() {
    // The dedup integrity invariant: an Indexer row with id=5 and a
    // UserFeed row with id=5 must NOT collide. Without distinct
    // prefixes the same release showing up in both sources would
    // dedup to a single rss_seen entry, hiding the duplicate grab.
    // This is the actual user-visible failure mode the prefix exists
    // to prevent.
    let indexer = RssSource::Indexer {
        id: 5,
        name: "Animebytes".into(),
        kind: "torznab".into(),
    };
    let user_feed = RssSource::UserFeed {
        id: 5,
        name: "SubsPlease 1080p".into(),
    };
    assert_ne!(
        source_dedup_key(&indexer),
        source_dedup_key(&user_feed),
        "Indexer{{id:5}} and UserFeed{{id:5}} must produce distinct dedup keys"
    );
}

#[tokio::test]
async fn resolve_dispatch_for_item_returns_some_when_pool_has_default_client() {
    // Pin the `-> None` substitution of `resolve_dispatch_for_item`
    // by exercising the Nyaa branch against a pool with a default
    // torrent client. `client_for_nyaa_with_id(None)` falls back to
    // the torrent default, so the function should return Some(...).
    use crate::models::config::Config;
    use crate::services::download_client::DownloadClient;
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use std::collections::HashMap;
    use std::sync::Arc;

    // Minimal-DownloadClient stub defined below — the AppState pool
    // just needs ONE concrete entry so the `default_torrent_id =
    // Some(1)` fallback in `client_for_nyaa_with_id` resolves.
    let stub: Arc<dyn DownloadClient> = Arc::new(MinimalClient);
    let db = in_memory_pool().await;
    let state = build_test_app_state(db, Some(stub));
    let cfg = Config::default();
    let item = RssItem {
        title: "[Group] Show - 01 [1080p].mkv".into(),
        link: String::new(),
        guid: "guid-1".into(),
        torrent: String::new(),
        magnet: String::new(),
        info_hash: String::new(),
        group: String::new(),
        resolution: String::new(),
        is_batch: false,
        source: RssSource::Nyaa,
    };
    let pins: HashMap<i64, Option<i64>> = HashMap::new();
    let result = resolve_dispatch_for_item(&state, &cfg, &item, &pins).await;
    assert!(
        result.is_some(),
        "Nyaa item with default torrent client must resolve to Some(...)"
    );
}

/// Minimal DownloadClient impl — every method returns the cheapest
/// satisfying value. We never call any of these in this test file;
/// the AppState pool just needs ONE concrete entry so the
/// `default_torrent_id = Some(1)` fallback inside
/// `client_for_nyaa_with_id` resolves to it.
struct MinimalClient;

#[async_trait::async_trait]
impl crate::services::download_client::DownloadClient for MinimalClient {
    async fn test(&self) -> Result<String, String> {
        Ok("mock".into())
    }
    async fn add_torrent(
        &self,
        _url: &str,
        _hash: &str,
    ) -> Result<crate::services::download_client::AddOutcome, String> {
        Ok(crate::services::download_client::AddOutcome::Added)
    }
    async fn add_torrent_with_file_filter(
        &self,
        _url: &str,
        _hash: &str,
        _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<crate::services::download_client::SelectiveOutcome, String> {
        Ok(crate::services::download_client::SelectiveOutcome::FullDownload)
    }
    async fn list_scoped(
        &self,
    ) -> Result<Vec<crate::services::download_client::DownloadItem>, String> {
        Ok(vec![])
    }
    async fn get_files(
        &self,
        _hash: &str,
    ) -> Result<Vec<crate::services::download_client::DownloadFile>, String> {
        Ok(vec![])
    }
    async fn pause(&self, _hash: &str) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self, _hash: &str) -> Result<(), String> {
        Ok(())
    }
    async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
        Ok(())
    }
    async fn set_file_wanted(
        &self,
        _hash: &str,
        _files: &[usize],
        _wanted: bool,
    ) -> Result<(), String> {
        Ok(())
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
    fn protocol(&self) -> &'static str {
        "torrent"
    }
}
