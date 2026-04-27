//! Multi-client fan-out tests for `run_once`. PR F (multi-client) +
//! PR 109 (delete-cascade NULL-out) + PR 110 (in-loop NULL-cleanup
//! when the stamped client resolves to None) all touched the
//! resolution path that sits between `get_all_pending` and
//! `import_torrent`. Pre-this-test-file the only direct unit coverage
//! was the model-layer round-trip
//! (`set_download_client_round_trips_through_get_all_pending`),
//! which pins the schema column but not the behavior of the
//! per-grab fan-out.
//!
//! These tests sit at the same layer as the existing
//! `grab_sweep::tests::sweep_dispatches_to_pinned_client_not_default`
//! one — build a real `AppState` with a multi-client pool of
//! `RecordingClient`s, seed `pending_grabs` rows with specific
//! `download_client_id` stamps, drive `run_once`, and assert which
//! clients received `list_scoped` + the post-condition on the
//! grabbed_torrents row.
//!
//! Caveat: `cfg.post_processing_enabled` is left at the default
//! `false` so `run_once` early-returns into
//! `advance_state_without_import` before any fan-out happens. To
//! cover the fan-out path proper we set `post_processing_enabled =
//! true` AND `media_root` non-empty, and arrange the mock client
//! to return torrents whose `state_kind` is *not* complete — that
//! way `run_once` reaches the per-grab match block but never calls
//! `import_torrent`, which would touch the real filesystem.

use crate::models::download_clients::{DownloadClientForm, insert as insert_dc};
use crate::models::grabbed_torrents;
use crate::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};
use crate::services::post_processing;
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use async_trait::async_trait;

use super::POST_PROC_TEST_SERIALIZER;
use std::sync::Arc;
use std::sync::Mutex;

/// Recording mock that captures `list_scoped` calls and returns a
/// canned set of torrents per client. Mirrors the shape of
/// `grab_sweep::tests::RecordingClient` but tuned for what
/// `run_once` exercises (list_scoped only — the test never reaches
/// the import path).
struct RecordingClient {
    list_calls: Mutex<u32>,
    list_fails: bool,
    /// Canned response for `list_scoped`. Each entry maps to one
    /// `DownloadItem` returned with the given hash + state.
    canned: Vec<DownloadItem>,
}

impl RecordingClient {
    fn new(canned: Vec<DownloadItem>) -> Self {
        Self {
            list_calls: Mutex::new(0),
            list_fails: false,
            canned,
        }
    }

    fn failing() -> Self {
        Self {
            list_calls: Mutex::new(0),
            list_fails: true,
            canned: Vec::new(),
        }
    }

    fn list_call_count(&self) -> u32 {
        *self.list_calls.lock().unwrap()
    }
}

#[async_trait]
impl DownloadClient for RecordingClient {
    async fn test(&self) -> Result<String, String> {
        Ok("mock".into())
    }
    async fn add_torrent(&self, _url: &str, _hash: &str) -> Result<AddOutcome, String> {
        Ok(AddOutcome::Added)
    }
    async fn add_torrent_with_file_filter(
        &self,
        _url: &str,
        _hash: &str,
        _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        Ok(SelectiveOutcome::FullDownload)
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        *self.list_calls.lock().unwrap() += 1;
        if self.list_fails {
            Err("simulated list_scoped failure".into())
        } else {
            Ok(self.canned.clone())
        }
    }
    async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
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
}

fn fake_torrent(hash: &str, state_kind: DownloadItemState) -> DownloadItem {
    DownloadItem {
        hash: hash.to_string(),
        name: format!("torrent-{hash}"),
        size: 1000,
        progress: 0.5,
        dlspeed: 0,
        state: format!("{state_kind:?}"),
        category: "anime".to_string(),
        eta: 0,
        save_path: String::new(),
        content_path: String::new(),
        state_kind,
    }
}

/// Seed minimum config so `run_once` doesn't early-return at the
/// disabled / empty-media-root gate. We deliberately keep
/// `post_processing_enabled = 1` and a non-empty `media_root` so
/// the fan-out path runs, but every mock torrent reports
/// `state_kind = Downloading` (incomplete) — that means `run_once`
/// hits the `if !torrent.state_kind.is_complete() { continue; }`
/// guard for every match and never enters `import_torrent`.
async fn seed_config(db: &sqlx::SqlitePool) {
    sqlx::query(
        "INSERT INTO config (id, post_processing_enabled, media_root) \
         VALUES (1, 1, '/tmp/test-media-root') \
         ON CONFLICT(id) DO UPDATE SET \
             post_processing_enabled = 1, \
             media_root = '/tmp/test-media-root'",
    )
    .execute(db)
    .await
    .expect("seed config row");
}

async fn install_pool(
    state: &crate::AppState,
    entries: Vec<(i64, Arc<dyn DownloadClient>, bool)>, // (id, client, is_default)
) {
    let mut clients: std::collections::HashMap<i64, Arc<dyn DownloadClient>> =
        std::collections::HashMap::new();
    let mut default_id = None;
    for (id, c, is_default) in entries {
        if is_default {
            default_id = Some(id);
        }
        clients.insert(id, c);
    }
    let pool = crate::DownloadClientPool {
        clients,
        default_id,
    };
    *state.download_clients.write().await = Arc::new(pool);
}

#[tokio::test]
async fn run_once_fans_out_list_scoped_per_pinned_client() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Two pending grabs, each pinned to a different client. Both
    // clients should receive exactly one `list_scoped` call. Pre-PR-F
    // the loop fanned out only against the default — only one of the
    // two clients would have been touched.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show A").await;
    // Grab pinned to client-id 2 (Seedbox), hash deadbeef0
    let g1 = grabbed_torrents::record_grab(&db, "deadbeef0", "rel-1", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g1, Some(2))
        .await
        .unwrap();
    // Grab pinned to client-id 3 (alternate), hash deadbeef1
    let g2 = grabbed_torrents::record_grab(&db, "deadbeef1", "rel-2", series_id, &[2], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g2, Some(3))
        .await
        .unwrap();
    // Seed download_clients rows so the pool can resolve.
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default qbit",
            kind: "qbittorrent",
            url: "http://qbit",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "seedbox",
            kind: "deluge",
            url: "http://seedbox",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        },
    )
    .await
    .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "alt",
            kind: "transmission",
            url: "http://alt",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    let seedbox = Arc::new(RecordingClient::new(vec![fake_torrent(
        "deadbeef0",
        DownloadItemState::Downloading,
    )]));
    let alt = Arc::new(RecordingClient::new(vec![fake_torrent(
        "deadbeef1",
        DownloadItemState::Downloading,
    )]));
    install_pool(
        &state,
        vec![
            (1, default_client.clone() as Arc<dyn DownloadClient>, true),
            (2, seedbox.clone() as Arc<dyn DownloadClient>, false),
            (3, alt.clone() as Arc<dyn DownloadClient>, false),
        ],
    )
    .await;

    post_processing::run_once(&state).await;

    // Only the two pinned clients should have received list_scoped —
    // the default never sees the call because no pending grab points
    // at it (both grabs carry explicit pins).
    assert_eq!(
        seedbox.list_call_count(),
        1,
        "seedbox (id=2) must receive its single fan-out list_scoped call"
    );
    assert_eq!(
        alt.list_call_count(),
        1,
        "alt (id=3) must receive its single fan-out list_scoped call"
    );
    assert_eq!(
        default_client.list_call_count(),
        0,
        "default (id=1) must NOT see list_scoped — no pending grab references it. \
         Pre-PR-F the fan-out hit only the default; this assertion catches a regression \
         that would silently drop pinned-client grabs back onto the default."
    );
}

#[tokio::test]
async fn run_once_cleans_orphan_stamps_when_no_default_client_exists() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // PR 112 review #3 (4th pass) — the pre-pass orphan cleanup at
    // post_processing/mod.rs exists specifically for the case where
    // every pending grab points at a gone client AND there's no
    // default to fall back to. The pre-existing
    // `run_once_nulls_stamp_when_client_id_no_longer_in_pool` test
    // seeds a default, which masks this code path entirely (the
    // fan-out's `else if let Some(id) = default_id_opt` branch
    // saves the day). Without this test, a future refactor that
    // moves the cleanup back inside the loop would silently regress
    // — the grab would stay orphaned forever and never reach the
    // stale-grab pruning path.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g =
        grabbed_torrents::record_grab(&db, "all_orphan_no_default", "rel", series_id, &[1], false)
            .await
            .unwrap()
            .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(999))
        .await
        .unwrap();

    let state = build_test_app_state(db.clone(), None);
    // Empty pool: no clients, no default. This is the genuine
    // "all-orphans-no-default" shape the pre-pass defends.
    install_pool(&state, Vec::new()).await;

    post_processing::run_once(&state).await;

    let stamp_after: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(
        stamp_after.is_none(),
        "stamp must be NULLed even when no default client exists. \
         Without the pre-pass cleanup the fan-out's `clients.is_empty()` \
         early-return would skip the per-loop NULL and orphan the grab forever."
    );
}

#[tokio::test]
async fn run_once_nulls_stamp_when_client_id_no_longer_in_pool() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // A grab stamped with `download_client_id = 999` (a deleted /
    // never-existed client). The PR 110 in-loop cleanup must NULL the
    // stamp so the next pass falls through to default and the grab
    // can either match or hit the 60s stale path. Pre-fix the grab
    // was orphaned forever (run_once `continue`s past the stale check
    // on resolve-fail).
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "orphaned", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(999))
        .await
        .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    // The grab's stamp must have been NULLed so a later pass falls
    // through to default + the stale path.
    let stamp_after: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(
        stamp_after.is_none(),
        "stamp must be NULLed when the referenced client is gone from the pool. \
         Pre-PR-110 fix: the grab orphaned forever (continue past the stale check)."
    );
}

#[tokio::test]
async fn run_once_does_not_null_stamp_on_transient_list_scoped_failure() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Sister case to the orphan-cleanup test: when a stamped client
    // IS in the pool but its `list_scoped` fails this pass (network
    // glitch, transient 5xx), the stamp must stay so the next pass
    // retries the same client. Pre-PR-110 the in-loop cleanup
    // didn't distinguish these cases — a refactor that NULLs on every
    // resolve-fail would silently fall back to default, which is the
    // opposite of what we want for transient failures.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g = grabbed_torrents::record_grab(&db, "transient", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g, Some(2))
        .await
        .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();
    insert_dc(
        &db,
        DownloadClientForm {
            name: "flaky",
            kind: "deluge",
            url: "http://flaky",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: false,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    let flaky = Arc::new(RecordingClient::failing());
    install_pool(
        &state,
        vec![
            (1, default_client.clone() as Arc<dyn DownloadClient>, true),
            (2, flaky.clone() as Arc<dyn DownloadClient>, false),
        ],
    )
    .await;

    post_processing::run_once(&state).await;

    // The flaky client was queried (and failed), the stamp was NOT
    // NULLed because the client is still in the pool — only the
    // not-in-pool case clears the stamp.
    assert_eq!(flaky.list_call_count(), 1);
    let stamp_after: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        stamp_after,
        Some(2),
        "stamp must survive a transient list_scoped failure so the next pass retries \
         the same client. NULLing here would silently fall back to default."
    );
}

#[tokio::test]
async fn run_once_isolates_failures_per_client() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Two pinned grabs on two different clients; one client's
    // list_scoped fails, the other's succeeds. The successful
    // client's grab must still be processed — failures don't poison
    // the cross-client fan-out. Pre-PR-F this couldn't even happen
    // (single client meant one failure killed the whole pass).
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let g_ok = grabbed_torrents::record_grab(&db, "okhash", "ok-rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g_ok, Some(2))
        .await
        .unwrap();
    let g_fail = grabbed_torrents::record_grab(&db, "failhash", "fail-rel", series_id, &[2], false)
        .await
        .unwrap()
        .unwrap();
    grabbed_torrents::set_download_client(&db, g_fail, Some(3))
        .await
        .unwrap();
    for (name, kind, url, is_default) in [
        ("default", "qbittorrent", "http://q", true),
        ("ok-client", "deluge", "http://ok", false),
        ("fail-client", "transmission", "http://fail", false),
    ] {
        insert_dc(
            &db,
            DownloadClientForm {
                name,
                kind,
                url,
                username: "",
                password: "",
                label: "",
                download_path: "",
                enabled: true,
                is_default,
            },
        )
        .await
        .unwrap();
    }

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(Vec::new()));
    let ok_client = Arc::new(RecordingClient::new(vec![fake_torrent(
        "okhash",
        DownloadItemState::Downloading,
    )]));
    let fail_client = Arc::new(RecordingClient::failing());
    install_pool(
        &state,
        vec![
            (1, default_client.clone() as Arc<dyn DownloadClient>, true),
            (2, ok_client.clone() as Arc<dyn DownloadClient>, false),
            (3, fail_client.clone() as Arc<dyn DownloadClient>, false),
        ],
    )
    .await;

    post_processing::run_once(&state).await;

    // Both pinned clients were queried (fan-out ran in full); the
    // failing client's grab is left pending (stamp survives, no
    // stale-mark since the client IS in the pool); the successful
    // client's grab matched its torrent and reached the
    // is_complete() guard (which short-circuits since the mock
    // torrent is `Downloading`, never `Seeding`).
    assert_eq!(ok_client.list_call_count(), 1, "ok client must run");
    assert_eq!(fail_client.list_call_count(), 1, "fail client must run");
    // Both grabs survive — neither matched a complete torrent, but
    // that's the in-flight expectation. Stamps stay.
    let ok_stamp: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g_ok)
            .fetch_one(&db)
            .await
            .unwrap();
    let fail_stamp: Option<i64> =
        sqlx::query_scalar("SELECT download_client_id FROM grabbed_torrents WHERE id = ?")
            .bind(g_fail)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        ok_stamp,
        Some(2),
        "ok grab stamp survives — its client is healthy"
    );
    assert_eq!(
        fail_stamp,
        Some(3),
        "fail grab stamp survives — transient list_scoped error doesn't NULL"
    );
}

#[tokio::test]
async fn run_once_falls_back_to_default_for_null_stamps() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    // Legacy / unstamped grab (download_client_id IS NULL). The
    // resolution chain must fall through to default — preserves
    // pre-multi-client behavior for upgraders whose existing grabs
    // never went through `set_download_client`.
    let db = in_memory_pool().await;
    seed_config(&db).await;
    let series_id = seed_series(&db, 1, "Show").await;
    let _g = grabbed_torrents::record_grab(&db, "legacy", "rel", series_id, &[1], false)
        .await
        .unwrap()
        .unwrap();
    // Deliberately do NOT call set_download_client — leave column NULL.
    insert_dc(
        &db,
        DownloadClientForm {
            name: "default",
            kind: "qbittorrent",
            url: "http://q",
            username: "",
            password: "",
            label: "",
            download_path: "",
            enabled: true,
            is_default: true,
        },
    )
    .await
    .unwrap();

    let state = build_test_app_state(db.clone(), None);
    let default_client = Arc::new(RecordingClient::new(vec![fake_torrent(
        "legacy",
        DownloadItemState::Downloading,
    )]));
    install_pool(
        &state,
        vec![(1, default_client.clone() as Arc<dyn DownloadClient>, true)],
    )
    .await;

    post_processing::run_once(&state).await;

    // Default client received the call (NULL stamp → default
    // fall-through), and the grab's stamp stays NULL since the
    // client IS in the pool (transient or fall-through, not deleted).
    assert_eq!(default_client.list_call_count(), 1);
}
