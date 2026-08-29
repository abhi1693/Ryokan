//! Live end-to-end batch-import proof for the PR #198 guards. Unlike
//! the `run_once.rs` fan-out tests (which stop short of
//! `import_torrent`), these drive the FULL import path — readiness
//! gate → batch preflight → import loop → real `do_file_op` calls on
//! a real temp filesystem — with only the download client mocked.
//!
//! Three guards under proof:
//!   1. Unparseable extras (NCOP/NCED) are skipped while the
//!      parseable episodes import, including a dot-delimited name
//!      whose `H.264` token must not mis-parse as episode 264.
//!   2. Two files resolving to one destination slot fail the whole
//!      grab before any mutation (nothing lands in the library,
//!      sources untouched).
//!   3. An incomplete wanted video holds the entire batch in
//!      `pending` (readiness gate) rather than importing a subset.

use crate::models::grabbed_torrents;
use crate::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};
use crate::services::post_processing;
use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::POST_PROC_TEST_SERIALIZER;

/// Minimal mock: one canned complete torrent plus a canned file list.
/// Everything else is inert. `save_path` points at a real temp dir the
/// test populated, so `import_torrent` walks and moves real files.
struct BatchClient {
    torrent: DownloadItem,
    files: Vec<DownloadFile>,
}

#[async_trait]
impl DownloadClient for BatchClient {
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
        Ok(vec![self.torrent.clone()])
    }
    async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(self.files.clone())
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

fn complete_torrent(hash: &str, save_path: &Path) -> DownloadItem {
    DownloadItem {
        hash: hash.to_string(),
        name: format!("torrent-{hash}"),
        size: 1000,
        progress: 1.0,
        dlspeed: 0,
        state: "seeding".to_string(),
        category: "anime".to_string(),
        eta: 0,
        save_path: save_path.to_string_lossy().into_owned(),
        content_path: String::new(),
        state_kind: DownloadItemState::Seeding,
    }
}

fn complete_file(name: &str) -> DownloadFile {
    DownloadFile {
        name: name.to_string(),
        size: 100,
        progress: 1.0,
        wanted: true,
    }
}

/// Fresh per-test temp tree: `<tmp>/ryokan-live-<tag>-<pid>/{downloads,media}`.
/// Any stale tree from a crashed prior run is cleared first.
fn temp_dirs(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("ryokan-live-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let downloads = root.join("downloads");
    let media = root.join("media");
    std::fs::create_dir_all(&downloads).expect("create downloads dir");
    std::fs::create_dir_all(&media).expect("create media dir");
    (root, downloads, media)
}

async fn seed_config(db: &sqlx::SqlitePool, media_root: &Path) {
    sqlx::query(
        "INSERT INTO config (id, post_processing_enabled, media_root) \
         VALUES (1, 1, ?) \
         ON CONFLICT(id) DO UPDATE SET post_processing_enabled = 1, media_root = excluded.media_root",
    )
    .bind(media_root.to_string_lossy().as_ref())
    .execute(db)
    .await
    .expect("seed config row");
}

/// Recursively collect basenames of files with the given extension.
fn collect_files(root: &Path, ext: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

async fn grab_state(db: &sqlx::SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM grabbed_torrents WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await
        .expect("fetch grab state")
}

#[tokio::test]
async fn live_batch_import_skips_extras_and_imports_episodes() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("extras");

    // Real files on disk, shapes lifted from the scraped Nyaa corpus:
    // three dash-form episodes, one dot-form episode whose `H.264`
    // token must not mis-parse as E264, and two NC extras that parse
    // to `None` by design.
    let names = [
        "[Moozzi2] Anne of Green Gables - 01 (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables - 02 (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables - 03 (BD 1440x1080 x.265 Flac).mkv",
        "Anne.of.Green.Gables.04.BD.1080p.H.264.mkv",
        "[Moozzi2] Anne of Green Gables [SP01] NCOP (BD 1440x1080 x.265 Flac).mkv",
        "[Moozzi2] Anne of Green Gables [SP02] NCED (BD 1440x1080 x.265 Flac).mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9001, "Anne of Green Gables").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-extras",
        "[Moozzi2] Anne of Green Gables (TV + SP)",
        series_id,
        &[1, 2, 3, 4],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient {
        torrent: complete_torrent("livehash-extras", &downloads),
        files: names.iter().map(|n| complete_file(n)).collect(),
    });
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    let imported = collect_files(&media, "mkv");
    assert_eq!(
        imported,
        vec![
            "Anne of Green Gables - S01E01.mkv".to_string(),
            "Anne of Green Gables - S01E02.mkv".to_string(),
            "Anne of Green Gables - S01E03.mkv".to_string(),
            "Anne of Green Gables - S01E04.mkv".to_string(),
        ],
        "episodes 1-4 import (dot-form E04 must not become E264); NC extras stay out"
    );
    // Hardlink mode: sources remain for continued seeding.
    assert_eq!(collect_files(&downloads, "mkv").len(), 6);
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_misclassified_multi_video_grab_uses_file_shape() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("misclassified");

    // The indexer recorded only episode 1 and did not mark this as a batch.
    // The actual wanted file list is authoritative: both episodes must import,
    // and the dotted codec token must not turn episode 2 into episode 264.
    let names = [
        "Dragon.Ball.001.DVD.480p.mkv",
        "Dragon.Ball.002.DVD.480p.H.264.mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9004, "Dragon Ball").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-misclassified",
        "Dragon Ball complete pack",
        series_id,
        &[1],
        false,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient {
        torrent: complete_torrent("livehash-misclassified", &downloads),
        files: names.iter().map(|n| complete_file(n)).collect(),
    });
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        vec![
            "Dragon Ball - S01E01.mkv".to_string(),
            "Dragon Ball - S01E02.mkv".to_string(),
        ],
        "actual multi-video shape must override the stale single-episode grab metadata"
    );
    assert_eq!(grab_state(&db, grab_id).await, "imported");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_fails_closed_on_slot_collision() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("collision");

    // `05` and `05v2` both resolve to episode 5 — the whole grab must
    // fail before ANY mutation, including the unambiguous E06.
    let names = [
        "Show - 05 (720p).mkv",
        "Show - 05v2 (1080p).mkv",
        "Show - 06 (1080p).mkv",
    ];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9002, "Show").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-collision",
        "Show 05-06 pack",
        series_id,
        &[5, 6],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    let client = Arc::new(BatchClient {
        torrent: complete_torrent("livehash-collision", &downloads),
        files: names.iter().map(|n| complete_file(n)).collect(),
    });
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        Vec::<String>::new(),
        "duplicate destination must abort before any file lands"
    );
    assert_eq!(
        collect_files(&downloads, "mkv").len(),
        3,
        "sources untouched"
    );
    assert_eq!(grab_state(&db, grab_id).await, "failed");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_batch_import_waits_for_incomplete_wanted_video() {
    let _serializer = POST_PROC_TEST_SERIALIZER.lock().await;
    let (root, downloads, media) = temp_dirs("notready");

    let names = ["Show - 01 (1080p).mkv", "Show - 02 (1080p).mkv"];
    for name in &names {
        std::fs::write(downloads.join(name), b"fake video bytes").expect("write source file");
    }

    let db = in_memory_pool().await;
    seed_config(&db, &media).await;
    let series_id = seed_series(&db, 9003, "Show").await;
    let grab_id = grabbed_torrents::record_grab(
        &db,
        "livehash-notready",
        "Show pack",
        series_id,
        &[1, 2],
        true,
    )
    .await
    .unwrap()
    .unwrap();

    // E02 is wanted but only half done — the whole batch must wait.
    let mut files: Vec<DownloadFile> = names.iter().map(|n| complete_file(n)).collect();
    files[1].progress = 0.5;

    let client = Arc::new(BatchClient {
        torrent: complete_torrent("livehash-notready", &downloads),
        files,
    });
    let state = build_test_app_state(db.clone(), Some(client));

    post_processing::run_once(&state).await;

    assert_eq!(
        collect_files(&media, "mkv"),
        Vec::<String>::new(),
        "no partial import while a wanted video is incomplete"
    );
    assert_eq!(grab_state(&db, grab_id).await, "pending");

    let _ = std::fs::remove_dir_all(&root);
}
