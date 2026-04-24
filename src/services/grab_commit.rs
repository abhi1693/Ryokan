//! Shared grab-commit helper for issue #83's interactive file-picker.
//!
//! Two paths end up with a live, file-priority-applied, resumed torrent
//! that Ryokan wants to attribute to the library:
//!
//!   1. **User-confirmed** — `handlers::grab::grab_confirm` applies the
//!      user's selections and resumes the torrent. Filenames here are
//!      the user-kept subset (decision #7).
//!   2. **Walkaway auto-commit** — `services::grab_sweep::auto_commit_row`
//!      marks every file wanted on a heartbeat-lapsed row (decision #3).
//!      Filenames here are the full file list.
//!
//! Both paths need the same downstream work: write a `grabbed_torrents`
//! row so post-processing picks up the download, and run sibling
//! auto-expand so a batch pack's sequels/prequels/side-stories get
//! their own library rows and per-file routing.
//!
//! The auto-search path in `handlers::library::search` already does this
//! with a pre-computed classification from the scoring pipeline. The
//! interactive path doesn't — the modal doesn't run source
//! classification. We fall back to `ClassificationResult::unknown()`
//! and let post-processing backfill the real `(source, resolution,
//! is_remux)` verdict once files land on disk. `needs_review` flips
//! true on the unknown row so the classifier review page surfaces it
//! if post-processing can't confidently classify it either.

use sqlx::SqlitePool;

use crate::AppState;
use crate::models::grabbed_torrents;
use crate::models::log::LogCategory;
use crate::models::pending_grabs::PendingGrab;
use crate::services::anilist;
use crate::services::auto_expand::{self, AutoExpandGrabContext};
use crate::services::auto_search;
use crate::services::logger;
use crate::services::source::ClassificationResult;

/// Write the `grabbed_torrents` row and kick off sibling auto-expand
/// for a pending grab that's now live on the download client. Returns
/// `Some(grab_id)` on success, `None` when library attribution was
/// skipped (no series context, empty title, DB insert deduped against
/// an in-flight row).
///
/// `filenames` is the file list to feed into auto-expand — the
/// user-selected subset on the confirm path, the full list on the
/// auto-commit path. Auto-expand writes `grabbed_torrent_series`
/// routes only for files present in the list, so a subset here
/// correctly limits sibling library attribution to what's actually
/// going to import (decision #7's post-confirm timing shift).
///
/// Auto-expand fires as a `tokio::spawn` — the caller returns to the
/// HTTP response before the metadata-bound relation walk completes.
/// Post-processing has its own auto-expand safety net so a failure
/// here doesn't prevent eventual library attribution.
pub async fn commit_grab_and_expand(
    state: &AppState,
    row: &PendingGrab,
    filenames: Vec<String>,
    release_title: &str,
    is_batch: bool,
) -> Option<i64> {
    // Bare-magnet grab from the global search page with no series
    // context — we can't attribute the download to a library row, so
    // skip the write. The torrent still downloads (the caller already
    // issued the resume); it just won't show up in any series's
    // grabbed list. Post-processing will ignore it for the same
    // reason — no `grabbed_torrents` row to key off.
    let Some(series_id) = row.series_id else {
        logger::debug(
            &state.db,
            LogCategory::Grab,
            "skipping grab-row write — no series_id on pending grab",
            &row.info_hash,
        )
        .await;
        return None;
    };

    if release_title.trim().is_empty() {
        logger::warn(
            &state.db,
            LogCategory::Grab,
            "skipping grab-row write — release title empty",
            &row.info_hash,
        )
        .await;
        return None;
    }

    // Derive episode numbers from the release title. `parse_release_numbers`
    // handles single-episode (`... - 05 ...`), range (`01-12`), and
    // absolute-numbered (`25-48` for JoJo P3 Egypt-hen) forms. If the
    // title doesn't parse, fall back to an empty vec — post-processing
    // still classifies imported files individually, and the series
    // page's per-episode grab state backfills when `episode_tags` rows
    // land via auto-expand / post-processing. Writing an empty slice
    // matches the auto_search path's behavior for releases where the
    // title parser returns nothing.
    let mut ep_nums: Vec<i32> = auto_search::parse_release_numbers(release_title)
        .into_iter()
        .collect();
    ep_nums.sort_unstable();

    let grab_id = match grabbed_torrents::record_grab(
        &state.db,
        &row.info_hash,
        release_title,
        series_id,
        &ep_nums,
        is_batch,
    )
    .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            // Dedup hit against an in-flight `pending` row — another
            // flow is mid-commit on this hash. Don't stomp it; the
            // other flow will drive auto-expand. This matches the
            // auto_search path's `grab_id.flatten()` behavior.
            logger::debug(
                &state.db,
                LogCategory::Grab,
                "grab dedup hit — skipping auto-expand",
                &row.info_hash,
            )
            .await;
            return None;
        }
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::Grab,
                "record_grab failed",
                &format!("{} ({})", e, row.info_hash),
            )
            .await;
            return None;
        }
    };

    // Fire-and-forget the sibling auto-expand. Don't block the HTTP
    // response — the transitive relation walk can take several
    // seconds on cold DETAIL_CACHE, and confirm / auto-commit both
    // want to return as soon as the client-side work is done.
    //
    // A failed fetch or panic inside the spawn drops the auto-expand
    // silently; the import-time call in `services::post_processing`
    // is the safety net that guarantees siblings land eventually.
    if !filenames.is_empty() {
        let db_task = state.db.clone();
        let title_task = release_title.to_string();
        let ep_nums_task = ep_nums.clone();
        let info_hash_task = row.info_hash.clone();
        tokio::spawn(async move {
            run_auto_expand(
                db_task,
                info_hash_task,
                series_id,
                ep_nums_task,
                grab_id,
                title_task,
                filenames,
            )
            .await;
        });
    }

    Some(grab_id)
}

/// Resolve the parent series's AL detail and invoke `expand_from_files`.
/// Broken out of the spawn so the `?`-style control flow reads cleanly
/// without making every step inside the spawn an `if let Some` ladder.
async fn run_auto_expand(
    db: SqlitePool,
    info_hash: String,
    series_id: i64,
    ep_nums: Vec<i32>,
    grab_id: i64,
    title: String,
    filenames: Vec<String>,
) {
    // Look up anilist_id from series_id. Negative AL IDs (MAL-fallback
    // sentinel per CLAUDE.md) route to Jikan inside
    // `get_anime_detail_with_options`, which correctly returns an
    // AnimeDetail for display purposes but carries the negated id —
    // sibling detection still runs against that shape since
    // expand_from_files already filters `parent_detail.id <= 0`
    // internally.
    let anilist_id =
        match sqlx::query_scalar::<_, i64>("SELECT anilist_id FROM series WHERE id = ?")
            .bind(series_id)
            .fetch_optional(&db)
            .await
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                logger::warn(
                    &db,
                    LogCategory::Grab,
                    "auto-expand: series row vanished between grab-row write and detail fetch",
                    &format!("series_id={} hash={}", series_id, info_hash),
                )
                .await;
                return;
            }
            Err(e) => {
                logger::warn(
                    &db,
                    LogCategory::Grab,
                    "auto-expand: DB error resolving series_id",
                    &format!("{} ({})", e, info_hash),
                )
                .await;
                return;
            }
        };

    let detail = match anilist::get_anime_detail_with_options(anilist_id, None, false).await {
        Ok(d) => d,
        Err(e) => {
            logger::info(
                &db,
                LogCategory::Grab,
                "auto-expand: AL detail fetch failed; post-processing will retry at import time",
                &format!("{} ({})", e, info_hash),
            )
            .await;
            return;
        }
    };

    let ctx = AutoExpandGrabContext {
        classification: ClassificationResult::unknown(),
        release_group: String::new(),
        size_bytes: 0,
    };
    let _ = auto_expand::expand_from_files(
        &db, &filenames, &detail, series_id, &ep_nums, grab_id, &title, &ctx,
    )
    .await;
}
