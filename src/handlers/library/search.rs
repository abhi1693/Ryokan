//! Search/grab/auto-expand handlers for the library section.
//!
//! Split out of `handlers::library::mod` — this is by far the largest
//! slice of the library submodule: AniList title search, the auto-search
//! and interactive-search entry points (per-episode + batch), the grab
//! handlers, and the Phase 2 auto-expand pipeline that detects sibling
//! entries inside megapack torrents and routes per-file downloads to
//! each sibling's folder. Tests for the auto-expand and cumulative-
//! offset paths live here too.

use axum::{
    extract::{Path, Query, State},
    response::Json,
};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, monitoring, series};
use crate::services::{anilist, auto_search, logger, media, progress};

use super::AnilistSearchQuery;
use super::reconcile::{
    force_mal_fallback_enabled, maybe_hydrate_cumulative_offset, resolve_series_context,
};

#[utoipa::path(
    get,
    path = "/api/anilist/search",
    tag = "Library",
    summary = "Search AniList for anime",
    description = "Search for anime by title. Uses AniList as primary source with MAL/Jikan and Kitsu as fallbacks.",
    params(AnilistSearchQuery),
    responses(
        (status = 200, description = "Search results", body = Vec<anilist::AnimeEntry>),
        (status = 500, description = "Search failed"),
    ),
)]
pub async fn anilist_search(
    State(state): State<AppState>,
    Query(params): Query<AnilistSearchQuery>,
) -> Result<Json<Vec<anilist::AnimeEntry>>, (axum::http::StatusCode, String)> {
    // Per-search override (?source=al|mal) takes precedence over the
    // ambient config flag. Only `al`, `mal`, or omitted are valid —
    // surface a 400 on anything else so a client with a typo in its
    // query param doesn't silently drop into the config default and
    // look like the toggle is broken.
    let force_fallback = match params.source.as_deref() {
        Some("mal") => true,
        Some("al") => false,
        None => force_mal_fallback_enabled(&state.db).await,
        Some("") => force_mal_fallback_enabled(&state.db).await,
        Some(other) => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                format!(
                    "invalid source override: {:?} (expected \"al\", \"mal\", or omit)",
                    other
                ),
            ));
        }
    };
    let results = anilist::search_anime_with_options(&params.q, force_fallback)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let source = if results.iter().any(|r| r.source == "mal") {
        "MAL/Jikan fallback"
    } else {
        "AniList"
    };
    logger::info(
        &state.db,
        LogCategory::AniList,
        &format!("Title search: '{}'", params.q),
        &format!(
            "results={}, source={}, forced_fallback={}, requested={}",
            results.len(),
            source,
            force_fallback,
            params.source.as_deref().unwrap_or("(config default)"),
        ),
    )
    .await;

    Ok(Json(results))
}

#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}",
    tag = "Library",
    summary = "Get series detail",
    description = "Returns full metadata for a series by its AniList ID or internal database ID.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Series detail", body = anilist::AnimeDetail),
        (status = 500, description = "Failed to fetch detail"),
    ),
)]
pub async fn api_series_detail(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<anilist::AnimeDetail>, (axum::http::StatusCode, String)> {
    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(detail))
}

/// Phase 2: sibling auto-expansion for multi-series batch releases.
///
/// Called after a successful grab when the user targeted a series
/// that belongs to a franchise. Detects sibling entries
/// (sequels, prequels, side stories, etc.) in the torrent's file
/// list, upserts each detected sibling into the tracked series
/// table, and writes `grabbed_torrent_series` route rows so
/// post-processing can move each file into the correct sibling's
/// media folder.
///
/// Gated on AniList provenance (enforced inside
/// [`auto_search::detect_sibling_entries_in_pack`]) — Jikan-sourced
/// details are skipped to avoid duplicate library rows from MAL's
/// finer-grained splits (e.g. JoJo Stone Ocean, which MAL splits
/// into 3 cours but AL keeps as a single entry). When AL is down at
/// grab time the grab itself still succeeds; sibling detection just
/// returns an empty vec and the 12h metadata refresh can
/// retroactively run detection later if needed.
///
/// Errors here are soft: the grab has already completed and
/// post-processing's legacy fallback (no route rows → route
/// everything to `parent_series_id`) keeps the grab functional even
/// when auto-expand fails partway through. Any failures get logged
/// at warn level.
///
/// Resolve the list of episode numbers a batch release should be
/// recorded against at grab time. Parses explicit episode ranges
/// from the release title (e.g. "01-12", "E01-E24") via
/// [`auto_search::parse_release_numbers`], and when the title carries
/// no explicit numbers falls back to the series' known episode count
/// so a title like "Jellyfish Can't Swim in the Night" still spawns
/// a row per episode.
///
/// Used by the two batch grab handlers (`search_batch_releases` and
/// `grab_batch_result`). Without this, batch grabs passed an empty
/// episode list to `grabbed_torrents::record_grab` and skipped
/// `episode_tags::record_grab` entirely, which meant the series page
/// showed every episode as UNKNOWN until post-processing ran — and
/// if the user had post-processing disabled, the rows never got
/// created at all.
///
/// Capped at 1000 episodes as a safety rail against a garbage
/// AniList record reporting an absurd episode count.
fn batch_episode_numbers(title: &str, detail: &anilist::AnimeDetail) -> Vec<i32> {
    let mut ep_nums: Vec<i32> = auto_search::parse_release_numbers(title)
        .into_iter()
        .collect();
    if ep_nums.is_empty()
        && let Some(total) = detail.episodes
        && total > 0
        && total <= 1000
    {
        ep_nums = (1..=total).collect();
    }
    ep_nums.sort_unstable();
    ep_nums
}

// AutoExpandGrabContext + the core expansion logic live in
// `services::auto_expand` so `services::post_processing` can call the
// same routine as a fallback when the grab-time metadata wait here
// timed out. Re-export locally so call sites in this file stay terse.
use crate::services::auto_expand::{AutoExpandGrabContext, expand_from_files};

/// Grab-time outer orchestrator: wait for qBit metadata, then delegate
/// to [`services::auto_expand::expand_from_files`]. Failure here
/// (timeout, qBit error) is no longer load-bearing — post-processing
/// retries the same expansion at import time via
/// [`services::auto_expand::expand_from_files`], so a slow tracker that
/// can't deliver metadata in 3 minutes will still get sibling detection
/// once the torrent completes.
///
/// 180s ceiling (vs the 10s used by the interactive selective-narrowing
/// path) because this runs inside a `tokio::spawn` — blocking a few
/// minutes in the background is fine, the HTTP handler already
/// returned. A slow-DHT magnet or a public tracker under load can take
/// that long to fetch metadata.
#[allow(clippy::too_many_arguments)]
async fn auto_expand_library_from_pack(
    db: &SqlitePool,
    client: std::sync::Arc<dyn crate::services::download_client::DownloadClient>,
    info_hash: &str,
    parent_detail: &anilist::AnimeDetail,
    parent_series_id: i64,
    parent_episode_numbers: &[i32],
    grab_id: i64,
    torrent_title: &str,
    grab_ctx: &AutoExpandGrabContext,
) -> usize {
    if parent_detail.id <= 0 || info_hash.is_empty() {
        return 0;
    }

    let files = match crate::services::download_client::wait_for_files(
        &*client,
        info_hash,
        std::time::Duration::from_secs(180),
    )
    .await
    {
        Ok(files) => files,
        Err(e) => {
            logger::info(
                db,
                LogCategory::Library,
                &format!(
                    "auto-expand: grab-time metadata wait failed for '{}', post-processing will retry at import time",
                    torrent_title
                ),
                &e,
            )
            .await;
            return 0;
        }
    };
    let filenames: Vec<String> = files.iter().map(|f| f.name.clone()).collect();

    expand_from_files(
        db,
        &filenames,
        parent_detail,
        parent_series_id,
        parent_episode_numbers,
        grab_id,
        torrent_title,
        grab_ctx,
    )
    .await
}

pub(super) async fn run_auto_search_targets(
    state: &AppState,
    request_id: i64,
    targets: Vec<auto_search::SearchTarget>,
    allow_batch: bool,
    series_id: Option<i64>,
) -> Result<auto_search::AutoSearchReport, (axum::http::StatusCode, String)> {
    run_auto_search_targets_with_upgrades(
        state,
        request_id,
        targets,
        allow_batch,
        series_id,
        std::collections::HashMap::new(),
    )
    .await
}

/// Optional `?progress_id=<opaque>` query string the frontend appends to
/// auto-search trigger calls. The handler binds it to a fresh job in the
/// progress registry and the worker task emits stage events into it for
/// the sticky toast on the page.
#[derive(Deserialize, Default)]
pub struct AutoSearchQuery {
    pub progress_id: Option<String>,
}

/// Pick a user-facing title for progress toasts. Prefers the English
/// title, falling back to romaji — the same fallback the logger
/// already uses elsewhere in this handler.
fn display_title_for_progress(detail: &anilist::AnimeDetail) -> &str {
    if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    }
}

/// Emit a terminal progress event summarizing the outcome of an
/// auto-search task. Called from inside the spawned task so the
/// `progress::EMITTER` task-local is in scope.
async fn emit_auto_search_terminal(
    result: &Result<auto_search::AutoSearchReport, (axum::http::StatusCode, String)>,
) {
    match result {
        Ok(report) => {
            let grabbed = report.grabbed.len();
            if grabbed > 0 {
                // Show titles for ≤3 grabs, otherwise just the count —
                // a 50-episode batch shouldn't paste a 50-line toast.
                let body = if grabbed <= 3 {
                    Some(
                        report
                            .grabbed
                            .iter()
                            .map(|h| format!("{}: {}", h.target_label, h.release_title))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                } else {
                    Some(format!("{} releases queued for download", grabbed))
                };
                progress::emit(
                    "done",
                    "success",
                    format!(
                        "Grabbed {} release{}",
                        grabbed,
                        if grabbed == 1 { "" } else { "s" }
                    ),
                    body,
                    true,
                )
                .await;
            } else if !report.skipped.is_empty() {
                progress::emit(
                    "done",
                    "warn",
                    "No releases grabbed",
                    Some(report.skipped.join("\n")),
                    true,
                )
                .await;
            } else {
                progress::emit(
                    "done",
                    "warn",
                    "Nothing to search",
                    Some("No targets matched the requested scope".into()),
                    true,
                )
                .await;
            }
        }
        Err((_, msg)) => {
            progress::emit(
                "error",
                "error",
                "Auto search failed",
                Some(msg.clone()),
                true,
            )
            .await;
        }
    }
}

async fn run_auto_search_targets_with_upgrades(
    state: &AppState,
    request_id: i64,
    targets: Vec<auto_search::SearchTarget>,
    allow_batch: bool,
    series_id: Option<i64>,
    upgrade_classifications: std::collections::HashMap<
        i32,
        crate::services::source::ClassificationResult,
    >,
) -> Result<auto_search::AutoSearchReport, (axum::http::StatusCode, String)> {
    let qbit = {
        let guard = state.download_client.read().await;
        guard
            .as_ref()
            .ok_or((
                axum::http::StatusCode::BAD_REQUEST,
                "Download client not configured".to_string(),
            ))?
            .clone()
    };

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let title = if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Auto search started for {}", title),
        &format!("{} target(s), allow_batch={}", targets.len(), allow_batch),
    )
    .await;
    progress::emit(
        "search",
        "info",
        format!("Searching {}", title),
        Some(format!(
            "{} target{}",
            targets.len(),
            if targets.len() == 1 { "" } else { "s" }
        )),
        false,
    )
    .await;

    // Clone the compiled-CF Arc out from under the read lock so the
    // scoring loop below runs without holding it.
    let cfs = state.custom_formats.read().await.clone();

    let mut grabbed = Vec::new();
    let mut skipped = Vec::new();
    let total_targets = targets.len();
    for (idx, target) in targets.into_iter().enumerate() {
        let label = auto_search::target_label(&target);
        let is_upgrade = matches!(&target, auto_search::SearchTarget::Episode(n) if upgrade_classifications.contains_key(n));
        progress::emit(
            "search",
            "info",
            if total_targets > 1 {
                format!("[{}/{}] {}", idx + 1, total_targets, label)
            } else {
                format!("Searching: {}", label)
            },
            None,
            false,
        )
        .await;
        match auto_search::find_best_for_target(
            &state.db,
            &detail,
            &cfg,
            &target,
            allow_batch,
            is_upgrade,
            &cfs,
        )
        .await
        {
            Some(result) => {
                // Classify up front so both upgrade-verification and log labels
                // read the same result.
                let incoming_classification = crate::services::source::classify_release(
                    &state.db,
                    &result.title,
                    Some(&result.resolution),
                    Some(crate::services::source::NyaaContext {
                        info_hash: &result.info_hash,
                        view_url: &result.link,
                        is_batch: result.is_batch,
                    }),
                    Some(crate::services::source::SeriesContext {
                        status: &detail.status,
                        season_year: detail.season_year,
                        end_year: detail.end_year,
                    }),
                )
                .await;

                // For upgrade targets, verify the found release is actually
                // better quality than what's already on disk.
                if let auto_search::SearchTarget::Episode(ep_num) = &target
                    && let Some(existing) = upgrade_classifications.get(ep_num)
                {
                    if incoming_classification.rank() <= existing.rank() {
                        logger::debug(
                            &state.db,
                            LogCategory::AutoSearch,
                            &format!(
                                "{}: skipped upgrade (incoming {} not better than existing {})",
                                label,
                                incoming_classification.label(),
                                existing.label()
                            ),
                            &result.title,
                        )
                        .await;
                        skipped.push(format!("{}: no quality upgrade available", label));
                        continue;
                    }
                    logger::info(
                        &state.db,
                        LogCategory::AutoSearch,
                        &format!(
                            "{}: upgrading from {} to {}",
                            label,
                            existing.label(),
                            incoming_classification.label()
                        ),
                        &result.title,
                    )
                    .await;
                }
                // For selective downloads, prefer the `.torrent` URL
                // over the magnet: qBit can parse metadata straight
                // from the file instead of waiting on DHT/trackers.
                let wants_selective = !result.info_hash.is_empty()
                    && auto_search::has_selective_discriminator(&detail);
                let url = if wants_selective && !result.torrent.is_empty() {
                    result.torrent.clone()
                } else if !result.magnet.is_empty() {
                    result.magnet.clone()
                } else {
                    result.torrent.clone()
                };
                if url.is_empty() {
                    logger::warn(
                        &state.db,
                        LogCategory::AutoSearch,
                        &format!("{}: no magnet/torrent URL", label),
                        &result.title,
                    )
                    .await;
                    skipped.push(format!("{}: no magnet/torrent URL", label));
                    continue;
                }
                // Selective-file path for multi-part / multi-season
                // packs. `pick_wanted_file_indices` narrows by part
                // number (Kizumonogatari II in a Monogatari megapack)
                // or positive subtitle match (Stardust Crusaders in a
                // JoJo S1–S5 pack). The gate only runs this branch
                // when the detail has an actual discriminator to try,
                // so single-entry series fall through to the plain
                // add. Franchise roots without their own subtitle
                // (JoJo S1) also fall through — they're handled by
                // the multi-series auto-expand path below, which
                // downloads the full pack and routes each sibling's
                // files to its own library entry. On filter error,
                // fall back to a full add rather than dropping the
                // grab entirely.
                let selective_outcome: Result<Option<Vec<usize>>, String> = if wants_selective {
                    let detail_clone = detail.clone();
                    let info_hash_clone = result.info_hash.clone();
                    let mut pick = move |files: &[String]| {
                        auto_search::pick_wanted_file_indices(files, &detail_clone)
                    };
                    match qbit
                        .add_torrent_with_file_filter(&url, &info_hash_clone, &mut pick)
                        .await
                    {
                        Ok(crate::services::download_client::SelectiveOutcome::Filtered(kept)) => {
                            Ok(Some(kept))
                        }
                        Ok(crate::services::download_client::SelectiveOutcome::FullDownload) => {
                            Ok(None)
                        }
                        Err(e) => {
                            logger::warn(
                                &state.db,
                                LogCategory::Grab,
                                &format!(
                                    "{}: selective download failed, falling back to full grab",
                                    label
                                ),
                                &e,
                            )
                            .await;
                            qbit.add_torrent(&url, &result.info_hash)
                                .await
                                .map(|_| None)
                        }
                    }
                } else {
                    qbit.add_torrent(&url, &result.info_hash)
                        .await
                        .map(|_| None)
                };
                match selective_outcome {
                    Ok(kept) => {
                        let selective_suffix = match (&kept, wants_selective) {
                            (Some(ids), _) => format!(", selective={}", ids.len()),
                            (None, true) => ", selective=full(timeout)".to_string(),
                            (None, false) => String::new(),
                        };
                        logger::info(
                            &state.db,
                            LogCategory::Grab,
                            &format!("Grabbed: {}", result.title),
                            &format!(
                                "target={}, group={}, score={}, tier={}, batch={}{}",
                                label,
                                result.group,
                                result.score,
                                incoming_classification.label(),
                                result.is_batch,
                                selective_suffix
                            ),
                        )
                        .await;
                        progress::emit(
                            "grab",
                            "success",
                            format!("Grabbed: {}", label),
                            Some(format!(
                                "{} [{}]",
                                result.title,
                                incoming_classification.label()
                            )),
                            false,
                        )
                        .await;
                        // Record for post-processing and episode quality tags.
                        if let Some(sid) = series_id {
                            let mut ep_nums: Vec<i32> = match &target {
                                auto_search::SearchTarget::Episode(n) => vec![*n],
                                auto_search::SearchTarget::Single => vec![1],
                            };
                            // For batch releases, parse all episode numbers from
                            // the title so every covered episode gets a grab tag.
                            if result.is_batch {
                                let parsed = auto_search::parse_release_numbers(&result.title);
                                if !parsed.is_empty() {
                                    ep_nums = parsed.into_iter().collect();
                                    ep_nums.sort_unstable();
                                }
                            }
                            let grab_id = crate::models::grabbed_torrents::record_grab(
                                &state.db,
                                &result.info_hash,
                                &result.title,
                                sid,
                                &ep_nums,
                                result.is_batch,
                            )
                            .await
                            .ok()
                            .flatten();
                            for ep_num in &ep_nums {
                                let _ = episode_tags::record_grab(
                                    &state.db,
                                    sid,
                                    *ep_num,
                                    &incoming_classification,
                                    &result.title,
                                    &result.group,
                                    result.size_bytes,
                                    result.is_batch,
                                )
                                .await;
                            }
                            // Phase 2 sibling auto-expand: when the
                            // grab is a batch covering a franchise
                            // (e.g. JoJo S1-S5), detect sibling
                            // entries in the file list and add them
                            // to the library so post-processing can
                            // route each file to the correct series.
                            // Only runs on fresh grabs (existing
                            // grab_id skips the route-write path).
                            //
                            // Skip auto-expand when selective narrowing
                            // successfully applied — the user is
                            // explicitly targeting one sibling inside a
                            // megapack (e.g. Stardust Crusaders in a
                            // JoJo pack), so the other siblings' files
                            // are marked priority=0 in qBit and will
                            // never land. Creating ghost library rows
                            // for them would leave dangling entries
                            // with no imported files. The
                            // `kept.is_none()` fallback path
                            // (selective filter timed out → full
                            // download) still auto-expands because the
                            // whole pack is actually downloading.
                            let selective_narrowed = wants_selective && kept.is_some();
                            if result.is_batch
                                && !selective_narrowed
                                && let Some(grab_id) = grab_id
                            {
                                // Fire-and-forget so the HTTP handler
                                // doesn't block up to ~60s waiting on
                                // the client to discover metadata (see
                                // the `wait_for_files` call inside
                                // `auto_expand_library_from_pack`).
                                // Failures here only affect post-
                                // processing routing, which already
                                // falls back to the parent series.
                                let db_task = state.db.clone();
                                let qbit_task = qbit.clone();
                                let info_hash_task = result.info_hash.clone();
                                let detail_task = detail.clone();
                                let ep_nums_task = ep_nums.clone();
                                let title_task = result.title.clone();
                                let grab_ctx_task = AutoExpandGrabContext {
                                    classification: incoming_classification.clone(),
                                    release_group: result.group.clone(),
                                    size_bytes: result.size_bytes,
                                };
                                tokio::spawn(async move {
                                    auto_expand_library_from_pack(
                                        &db_task,
                                        qbit_task,
                                        &info_hash_task,
                                        &detail_task,
                                        sid,
                                        &ep_nums_task,
                                        grab_id,
                                        &title_task,
                                        &grab_ctx_task,
                                    )
                                    .await;
                                });
                            }
                        }
                    }
                    Err(e) => {
                        logger::error(
                            &state.db,
                            LogCategory::QBit,
                            &format!("Failed to add torrent for {}", label),
                            &e,
                        )
                        .await;
                        return Err((axum::http::StatusCode::BAD_GATEWAY, e));
                    }
                }
                let queued_batch = result.is_batch;
                grabbed.push(auto_search::AutoSearchHit {
                    target_label: label.clone(),
                    release_title: result.title,
                    release_group: result.group,
                    quality_tier: incoming_classification.label(),
                    url,
                    score: result.score,
                });
                if queued_batch && allow_batch {
                    logger::info(
                        &state.db,
                        LogCategory::AutoSearch,
                        "Season pack queued; stopping episode search",
                        "",
                    )
                    .await;
                    skipped.push(
                        "Season pack queued; skipped additional episode searches".to_string(),
                    );
                    break;
                }
            }
            None => {
                logger::debug(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!("{}: no matching release found", label),
                    "",
                )
                .await;
                skipped.push(format!("{}: no matching release found", label));
            }
        }
    }

    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        &format!(
            "Auto search complete: {} grabbed, {} skipped",
            grabbed.len(),
            skipped.len()
        ),
        &format!("profile={}", cfg.quality_profile),
    )
    .await;

    Ok(auto_search::AutoSearchReport {
        grabbed,
        skipped,
        quality_profile: cfg.quality_profile,
    })
}

#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/auto-search",
    tag = "Library",
    summary = "Auto-search all episodes",
    description = "Automatically search and grab the best release for every monitored episode of a series.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Auto-search report", body = auto_search::AutoSearchReport),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn auto_search_series(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Query(q): Query<AutoSearchQuery>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let progress_handle = match progress::sanitize_progress_id(q.progress_id.as_deref()) {
        Some(id) => Some(state.progress.register(id).await),
        None => None,
    };
    if let Some(h) = &progress_handle {
        h.emit("start", "info", "Preparing auto-search…", None, false)
            .await;
    }
    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let (tracked_row, provider_id, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let tracked = if let Some(row) = tracked_row {
        Some(row)
    } else if let Some(mid) = detail.id_mal {
        series::get_by_mal_id(&state.db, mid)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        series::get_by_anilist_id(&state.db, provider_id)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let tracked = maybe_hydrate_cumulative_offset(&state.db, tracked, &detail).await;
    let folder_name = tracked
        .as_ref()
        .map(|s| s.folder_name.clone())
        .unwrap_or_default();
    let existing_files = media::scan_series_folder(&cfg.media_root, &folder_name);
    let existing_eps: Vec<i32> = existing_files.iter().map(|f| f.episode_number).collect();

    let monitored_eps = if let Some(ref tracked_series) = tracked {
        monitoring::get_monitored_episode_numbers(&state.db, tracked_series.id)
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        Vec::new()
    };

    let mut targets = if tracked.is_some() {
        auto_search::build_monitored_targets(&detail, &existing_eps, &monitored_eps)
    } else {
        auto_search::build_missing_targets(&detail, &existing_eps)
    };

    // Also include upgrade targets: episodes on disk below the quality cutoff.
    let (cutoff_source, cutoff_is_remux, cutoff_is_bdmv) =
        crate::services::source::parse_cutoff_source(&cfg.cutoff_source);
    let cutoff_resolution = crate::services::source::Resolution::from_str(&cfg.cutoff_resolution);
    let quality_tags = if let Some(ref t) = tracked {
        episode_tags::get_for_series(&state.db, t.id)
            .await
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    let upgrade_targets = auto_search::build_upgrade_targets(
        &existing_files,
        &monitored_eps,
        cutoff_source,
        cutoff_resolution,
        cutoff_is_remux,
        cutoff_is_bdmv,
        &quality_tags,
    );
    // Merge upgrade targets (avoid duplicates with missing targets).
    let existing_target_eps: std::collections::HashSet<i32> = targets
        .iter()
        .filter_map(|t| match t {
            auto_search::SearchTarget::Episode(n) => Some(*n),
            _ => None,
        })
        .collect();
    for (target, _) in &upgrade_targets {
        if let auto_search::SearchTarget::Episode(n) = target
            && !existing_target_eps.contains(n)
        {
            targets.push(target.clone());
        }
    }

    let target_summary = if targets.len() <= 5 {
        targets
            .iter()
            .map(auto_search::target_label)
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!("{} targets", targets.len())
    };
    let upgrade_count = upgrade_targets.len();
    let title_for_log = if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    };
    logger::debug(
        &state.db,
        LogCategory::AutoSearch,
        &format!("Missing targets for {}: {}", title_for_log, target_summary),
        &format!(
            "on_disk={}, monitored={}, upgradeable={}, total={:?}",
            existing_eps.len(),
            monitored_eps.len(),
            upgrade_count,
            detail.episodes
        ),
    )
    .await;
    let series_id_for_grab = tracked.as_ref().map(|s| s.id);
    // Build a map of existing episode classifications for upgrade verification in the search task.
    let upgrade_classifications: std::collections::HashMap<
        i32,
        crate::services::source::ClassificationResult,
    > = upgrade_targets
        .into_iter()
        .filter_map(|(t, classification)| match t {
            auto_search::SearchTarget::Episode(n) => Some((n, classification)),
            _ => None,
        })
        .collect();
    // Spawn as an independent task so the grab completes even if the client
    // disconnects. The spawned future is wrapped in `progress::scope` when a
    // progress handle was registered, so deep callees inside the search
    // pipeline can `progress::emit` into the toast without threading the
    // handle through every signature.
    let state_clone = state.clone();
    let progress_for_task = progress_handle.clone();
    let handle = tokio::spawn(progress::run_with_progress(progress_for_task, async move {
        let result = run_auto_search_targets_with_upgrades(
            &state_clone,
            request_id,
            targets,
            true,
            series_id_for_grab,
            upgrade_classifications,
        )
        .await;
        emit_auto_search_terminal(&result).await;
        result
    }));
    let report = handle.await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Search task failed: {}", e),
        )
    })??;
    Ok(Json(report))
}

#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/auto-search/{episode_number}",
    tag = "Library",
    summary = "Auto-search single episode",
    description = "Automatically search and grab the best release for a specific episode.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, description = "Auto-search report", body = auto_search::AutoSearchReport),
        (status = 400, description = "Invalid episode for media type"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn auto_search_episode(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Query(q): Query<AutoSearchQuery>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let progress_handle = match progress::sanitize_progress_id(q.progress_id.as_deref()) {
        Some(id) => Some(state.progress.register(id).await),
        None => None,
    };
    if let Some(h) = &progress_handle {
        h.emit(
            "start",
            "info",
            format!("Searching episode {}…", episode_number),
            None,
            false,
        )
        .await;
    }
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id_for_grab: Option<i64> = tracked_row.as_ref().map(|s| s.id);

    if let Some(_tracked) = tracked_row {
        // Monitoring status does not block manual episode searches.
    } else if matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA")
        && episode_number != 1
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "Single-entry media can only search episode 1".to_string(),
        ));
    }

    logger::debug(
        &state.db,
        LogCategory::AutoSearch,
        &format!(
            "Episode search: series_ref={}, episode={}",
            request_id, episode_number
        ),
        "allow_batch=false",
    )
    .await;
    // Collapse to Single for single-entry media so movie/OVA/special
    // release titles (which don't carry episode numbers) aren't filtered
    // out by the Episode(n) matching rules.
    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);

    // Spawn as an independent task so the grab completes even if the client
    // disconnects. The spawn is wrapped in `progress::run_with_progress`
    // when a progress handle was registered above so deep callees can emit
    // into the user's sticky toast without threading the handle down.
    let state_clone = state.clone();
    let progress_for_task = progress_handle.clone();
    let handle = tokio::spawn(progress::run_with_progress(progress_for_task, async move {
        let result = run_auto_search_targets(
            &state_clone,
            request_id,
            vec![target],
            false,
            series_id_for_grab,
        )
        .await;
        emit_auto_search_terminal(&result).await;
        result
    }));
    let report = handle.await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Search task failed: {}", e),
        )
    })??;
    Ok(Json(report))
}

/// Search batch releases only for a series (no single-episode grabs).
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/search-batch",
    tag = "Library",
    summary = "Search for batch releases",
    description = "Search for batch/complete-series torrent releases and grab the best match.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Batch search report", body = auto_search::AutoSearchReport),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn search_batch_releases(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Query(q): Query<AutoSearchQuery>,
) -> Result<Json<auto_search::AutoSearchReport>, (axum::http::StatusCode, String)> {
    let progress_handle = match progress::sanitize_progress_id(q.progress_id.as_deref()) {
        Some(id) => Some(state.progress.register(id).await),
        None => None,
    };
    if let Some(h) = &progress_handle {
        h.emit("start", "info", "Searching for batch release…", None, false)
            .await;
    }

    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id_for_grab = tracked_row.as_ref().map(|s| s.id);

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    if let Some(h) = &progress_handle {
        h.emit(
            "search",
            "info",
            format!("Searching: {}", display_title_for_progress(&detail)),
            None,
            false,
        )
        .await;
    }

    // Pick the best *batch* — filtering to is_batch pre-selection instead
    // of post-selection. The old code called find_best_for_target and
    // post-filtered, which returned None whenever the top-scored result
    // was a single-episode weekly release (i.e. almost every popular show
    // with active weekly seeders).
    let best = auto_search::find_best_batch_for_target(
        &state.db,
        &detail,
        &cfg,
        &auto_search::SearchTarget::Single,
        &cfs,
    )
    .await;

    let qbit = {
        let guard = state.download_client.read().await;
        guard
            .as_ref()
            .ok_or({
                if let Some(h) = &progress_handle {
                    // Fire-and-forget: we're about to Err-return, so the
                    // toast is the only surface that tells the user why.
                    let h = h.clone();
                    tokio::spawn(async move {
                        h.emit(
                            "error",
                            "error",
                            "Download client not configured",
                            None,
                            true,
                        )
                        .await;
                    });
                }
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    "Download client not configured".to_string(),
                )
            })?
            .clone()
    };

    match best {
        None => {
            if let Some(h) = &progress_handle {
                h.emit("done", "warn", "No batch release found", None, true)
                    .await;
            }
            Err((
                axum::http::StatusCode::NOT_FOUND,
                "No batch release found".to_string(),
            ))
        }
        Some(result) => {
            let url = if !result.magnet.is_empty() {
                result.magnet.clone()
            } else {
                result.torrent.clone()
            };
            if url.is_empty() {
                if let Some(h) = &progress_handle {
                    h.emit(
                        "error",
                        "error",
                        "No magnet/torrent URL for batch release",
                        None,
                        true,
                    )
                    .await;
                }
                return Err((
                    axum::http::StatusCode::BAD_GATEWAY,
                    "No magnet/torrent URL for batch release".to_string(),
                ));
            }
            if let Some(h) = &progress_handle {
                h.emit(
                    "grab",
                    "info",
                    format!("Grabbing {}", result.title),
                    None,
                    false,
                )
                .await;
            }
            qbit.add_torrent(&url, &result.info_hash)
                .await
                .map_err(|e| {
                    if let Some(h) = &progress_handle {
                        let h = h.clone();
                        let err = e.clone();
                        tokio::spawn(async move {
                            h.emit(
                                "error",
                                "error",
                                "qBittorrent rejected the torrent",
                                Some(err),
                                true,
                            )
                            .await;
                        });
                    }
                    (axum::http::StatusCode::BAD_GATEWAY, e)
                })?;
            let classification = crate::services::source::classify_release(
                &state.db,
                &result.title,
                Some(&result.resolution),
                Some(crate::services::source::NyaaContext {
                    info_hash: &result.info_hash,
                    view_url: &result.link,
                    is_batch: result.is_batch,
                }),
                Some(crate::services::source::SeriesContext {
                    status: &detail.status,
                    season_year: detail.season_year,
                    end_year: detail.end_year,
                }),
            )
            .await;
            let tier_label = classification.label();
            logger::info(
                &state.db,
                LogCategory::Grab,
                &format!("Grabbed batch: {}", result.title),
                &format!(
                    "group={}, score={}, tier={}",
                    result.group, result.score, tier_label
                ),
            )
            .await;
            if let Some(sid) = series_id_for_grab {
                // Parse episode list from the batch title so every covered
                // episode gets a per-episode `episode_quality_tags` row at
                // grab time, not just at post-processing time. Without
                // this the UI shows UNKNOWN for every episode of a
                // freshly-grabbed batch — and if the user has
                // post-processing disabled the rows never get created at
                // all. Mirrors the auto-search-path logic at
                // `run_auto_search_targets_with_upgrades` (look for
                // `parse_release_numbers` above).
                //
                // Fallback when the title carries no explicit range
                // (e.g. "Jellyfish Can't Swim in the Night" with no
                // "01-12" suffix): use the series' known episode count.
                // Capped at 1000 so a garbage AniList record can't
                // spawn a million rows.
                let ep_nums = batch_episode_numbers(&result.title, &detail);
                let _ = crate::models::grabbed_torrents::record_grab(
                    &state.db,
                    &result.info_hash,
                    &result.title,
                    sid,
                    &ep_nums,
                    result.is_batch,
                )
                .await;
                for ep_num in &ep_nums {
                    let _ = episode_tags::record_grab(
                        &state.db,
                        sid,
                        *ep_num,
                        &classification,
                        &result.title,
                        &result.group,
                        result.size_bytes,
                        result.is_batch,
                    )
                    .await;
                }
            }
            if let Some(h) = &progress_handle {
                h.emit(
                    "done",
                    "success",
                    "Batch grabbed",
                    Some(format!("{} ({})", result.title, tier_label)),
                    true,
                )
                .await;
            }
            Ok(Json(auto_search::AutoSearchReport {
                grabbed: vec![auto_search::AutoSearchHit {
                    target_label: "Batch".to_string(),
                    release_title: result.title,
                    release_group: result.group,
                    quality_tier: tier_label,
                    url,
                    score: result.score,
                }],
                skipped: vec![],
                quality_profile: cfg.quality_profile,
            }))
        }
    }
}

/// Interactive search: return all scored candidates for an episode without grabbing.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/interactive-search/{episode_number}",
    tag = "Library",
    summary = "Interactive episode search",
    description = "Search Nyaa for all available releases of a specific episode, returning scored results for manual selection.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number to search for"),
    ),
    responses(
        (status = 200, description = "Search results", body = Vec<crate::services::nyaa::SearchResult>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn interactive_search_episode(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
) -> Result<Json<Vec<crate::services::nyaa::SearchResult>>, (axum::http::StatusCode, String)> {
    // 5-minute TTL cache so rapid reloads of the picker modal during
    // UI iteration don't hammer Nyaa. Scope-limited to interactive
    // search only; auto-search / RSS / manual grabs still go direct.
    let cache_key = (request_id, Some(episode_number));
    if let Some(cached) =
        crate::services::interactive_search_cache::get(&state.interactive_search_cache, cache_key)
    {
        return Ok(Json((*cached).clone()));
    }

    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    // Same single-entry collapse as auto_search_episode — the interactive
    // picker otherwise returns zero results for movies.
    let target = auto_search::SearchTarget::for_episode(&detail, episode_number);
    let mut results =
        auto_search::find_all_for_target(&state.db, &detail, &cfg, &target, false, &cfs).await;

    // Layer 3 (group-map) enrichment. Auto-search already runs the full
    // source pipeline so its classification is complete, but the interactive
    // picker shows results straight from nyaa::parse_results where only
    // Layer 1 (anitomy filename tokens) has fired. Filling source via the
    // group table here is what lets SubsPlease releases label as WEB-DL
    // and VCB-Studio as BluRay when the filename alone is silent.
    crate::services::nyaa::enrich_results_with_group_map(&state.db, &mut results).await;

    crate::services::interactive_search_cache::insert(
        &state.interactive_search_cache,
        cache_key,
        results.clone(),
    );
    Ok(Json(results))
}

/// Interactive batch search: return all scored batch candidates so the user
/// can pick one. Uses the same query sweep as the auto batch search
/// (`find_best_batch_for_target`) so the interactive and auto paths surface
/// the same candidate pool — the only difference is that this returns every
/// hit instead of picking the top.
#[utoipa::path(
    get,
    path = "/api/series/{anilist_id}/interactive-search-batch",
    tag = "Library",
    summary = "Interactive batch search",
    description = "Search Nyaa for batch/complete releases of a series, returning scored results for manual selection.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    responses(
        (status = 200, description = "Batch search results", body = Vec<crate::services::nyaa::SearchResult>),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn interactive_search_batches(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Result<Json<Vec<crate::services::nyaa::SearchResult>>, (axum::http::StatusCode, String)> {
    // 5-minute TTL cache — see interactive_search_episode for rationale.
    // `None` episode slot distinguishes batch from per-episode.
    let cache_key = (request_id, None);
    if let Some(cached) =
        crate::services::interactive_search_cache::get(&state.interactive_search_cache, cache_key)
    {
        return Ok(Json((*cached).clone()));
    }

    let (_, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();

    let cfs = state.custom_formats.read().await.clone();

    let mut results = auto_search::collect_scored_batches_for_target(
        &state.db,
        &detail,
        &cfg,
        &auto_search::SearchTarget::Single,
        &cfs,
    )
    .await;

    crate::services::nyaa::enrich_results_with_group_map(&state.db, &mut results).await;

    crate::services::interactive_search_cache::insert(
        &state.interactive_search_cache,
        cache_key,
        results.clone(),
    );
    Ok(Json(results))
}

/// Grab a specific batch release chosen from interactive batch search.
///
/// Mirrors `grab_interactive_result` but without an episode number —
/// batches cover a range of episodes — the episode list is resolved
/// from the release title via [`batch_episode_numbers`] at grab time
/// so per-episode `episode_tags::record_grab` writes land immediately
/// and the UI shows the batch's quality tier without waiting on
/// post-processing.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/grab-batch",
    tag = "Library",
    summary = "Grab a specific batch release",
    description = "Send a specific batch torrent release (chosen from interactive batch search) to qBittorrent for download.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Batch grabbed", body = serde_json::Value),
        (status = 400, description = "No URL provided or qBittorrent not configured"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn grab_batch_result(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row.as_ref().map(|s| s.id);
    let url = body["url"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let group = body["group"].as_str().unwrap_or("").to_string();
    let resolution = body["resolution"].as_str().unwrap_or("").to_string();
    let info_hash = body["info_hash"].as_str().unwrap_or("").to_string();
    let size_bytes = body["size_bytes"].as_i64().unwrap_or(0);

    if url.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "No URL provided".to_string(),
        ));
    }

    let qbit = {
        let guard = state.download_client.read().await;
        guard
            .as_ref()
            .ok_or((
                axum::http::StatusCode::BAD_REQUEST,
                "Download client not configured".to_string(),
            ))?
            .clone()
    };

    // Same selective-file path as `grab_interactive_result`: narrow
    // a megapack to just the target if it has its own subtitle or
    // part number. Franchise roots (JoJo S1) deliberately fall
    // through so the multi-series auto-expand path below can route
    // each sibling's files into its own library entry instead.
    let wants_selective =
        !info_hash.is_empty() && auto_search::has_selective_discriminator(&detail);
    let selective_outcome: Option<Vec<usize>> = if wants_selective {
        let detail_clone = detail.clone();
        let mut pick =
            move |files: &[String]| auto_search::pick_wanted_file_indices(files, &detail_clone);
        match qbit
            .add_torrent_with_file_filter(&url, &info_hash, &mut pick)
            .await
        {
            Ok(crate::services::download_client::SelectiveOutcome::Filtered(kept)) => Some(kept),
            Ok(crate::services::download_client::SelectiveOutcome::FullDownload) => None,
            Err(e) => {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Selective batch download failed, falling back to full grab: {}",
                        title
                    ),
                    &e,
                )
                .await;
                qbit.add_torrent(&url, &info_hash)
                    .await
                    .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
                None
            }
        }
    } else {
        qbit.add_torrent(&url, &info_hash)
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
        None
    };

    // Classify so the log line carries the actual quality tier. Pass the
    // chosen release as a batch in NyaaContext (Layer 4 uses this for the
    // finished-series BluRay rule).
    let classification = crate::services::source::classify_release(
        &state.db,
        &title,
        Some(&resolution),
        Some(crate::services::source::NyaaContext {
            info_hash: &info_hash,
            view_url: "",
            is_batch: true,
        }),
        Some(crate::services::source::SeriesContext {
            status: &detail.status,
            season_year: detail.season_year,
            end_year: detail.end_year,
        }),
    )
    .await;

    let selective_suffix = match (&selective_outcome, wants_selective) {
        (Some(kept), _) => format!(", selective={}", kept.len()),
        (None, true) => ", selective=full(timeout)".to_string(),
        (None, false) => String::new(),
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!("Grabbed batch (interactive): {}", title),
        &format!(
            "group={}, tier={}{}",
            group,
            classification.label(),
            selective_suffix
        ),
    )
    .await;

    if let Some(sid) = series_id {
        // Parse episode list from the batch title so every covered
        // episode gets a per-episode `episode_quality_tags` row at
        // grab time. Same reasoning as in `search_batch_releases` —
        // without this, batch grabs leave every episode showing
        // UNKNOWN in the UI, and with post-processing disabled the
        // rows never get created at all.
        let ep_nums = batch_episode_numbers(&title, &detail);
        let grab_id = crate::models::grabbed_torrents::record_grab(
            &state.db, &info_hash, &title, sid, &ep_nums, true,
        )
        .await
        .ok()
        .flatten();
        for ep_num in &ep_nums {
            let _ = episode_tags::record_grab(
                &state.db,
                sid,
                *ep_num,
                &classification,
                &title,
                &group,
                size_bytes,
                true,
            )
            .await;
        }
        // Phase 2 sibling auto-expand. Skip when selective narrowing
        // successfully applied — the user picked a specific sibling
        // (e.g. Stardust Crusaders) out of a megapack and the other
        // siblings' files are marked priority=0 in qBit and won't
        // land. Creating library entries for them would leave ghost
        // rows with no imported files. The `selective_outcome.is_none()
        // && wants_selective` fallback (filter timed out → full
        // download) still auto-expands because the whole pack is
        // actually downloading.
        let selective_narrowed = wants_selective && selective_outcome.is_some();
        if !selective_narrowed && let Some(grab_id) = grab_id {
            // Fire-and-forget so the HTTP handler doesn't block
            // up to ~60s on qBit metadata discovery. See the
            // matching spawn in `run_auto_search_targets_with_upgrades`.
            let db_task = state.db.clone();
            let qbit_task = qbit.clone();
            let info_hash_task = info_hash.clone();
            let detail_task = detail.clone();
            let title_task = title.clone();
            let ep_nums_task = ep_nums.clone();
            let grab_ctx_task = AutoExpandGrabContext {
                classification: classification.clone(),
                release_group: group.clone(),
                size_bytes,
            };
            tokio::spawn(async move {
                auto_expand_library_from_pack(
                    &db_task,
                    qbit_task,
                    &info_hash_task,
                    &detail_task,
                    sid,
                    &ep_nums_task,
                    grab_id,
                    &title_task,
                    &grab_ctx_task,
                )
                .await;
            });
        }
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "title": title,
        "tier": classification.label(),
        "selective_files": selective_outcome,
    })))
}

/// Grab a specific release chosen from the interactive search.
#[utoipa::path(
    post,
    path = "/api/series/{anilist_id}/grab/{episode_number}",
    tag = "Library",
    summary = "Grab a specific release",
    description = "Send a specific torrent release (chosen from interactive search) to qBittorrent for download.",
    params(
        ("anilist_id" = i64, Path, description = "AniList ID or internal series ID"),
        ("episode_number" = i32, Path, description = "Episode number"),
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Release grabbed", body = serde_json::Value),
        (status = 400, description = "No URL provided or qBittorrent not configured"),
        (status = 502, description = "Metadata fetch failed"),
    ),
)]
pub async fn grab_interactive_result(
    State(state): State<AppState>,
    Path((request_id, episode_number)): Path<(i64, i32)>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let (tracked_row, _, detail) = resolve_series_context(&state.db, request_id)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;

    let series_id = tracked_row.as_ref().map(|s| s.id);
    let url = body["url"].as_str().unwrap_or("").to_string();
    let title = body["title"].as_str().unwrap_or("").to_string();
    let group = body["group"].as_str().unwrap_or("").to_string();
    let resolution = body["resolution"].as_str().unwrap_or("").to_string();
    let info_hash = body["info_hash"].as_str().unwrap_or("").to_string();
    let size_bytes = body["size_bytes"].as_i64().unwrap_or(0);

    if url.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "No URL provided".to_string(),
        ));
    }

    let qbit = {
        let guard = state.download_client.read().await;
        guard
            .as_ref()
            .ok_or((
                axum::http::StatusCode::BAD_REQUEST,
                "Download client not configured".to_string(),
            ))?
            .clone()
    };

    // If the target is a multi-part entry ("Kizumonogatari II") OR a
    // subtitled season of a franchise ("Stardust Crusaders"), try the
    // selective-file download path so a megapack release only pulls
    // the files the user is tracking. Franchise roots without their
    // own subtitle return `false` here and fall through to the plain
    // `add_torrent` path — interactive single-episode grabs don't
    // auto-expand the library (that's `grab_batch_result`'s job).
    let wants_selective =
        !info_hash.is_empty() && auto_search::has_selective_discriminator(&detail);
    let selective_outcome: Option<Vec<usize>> = if wants_selective {
        let detail_clone = detail.clone();
        let mut pick =
            move |files: &[String]| auto_search::pick_wanted_file_indices(files, &detail_clone);
        match qbit
            .add_torrent_with_file_filter(&url, &info_hash, &mut pick)
            .await
        {
            Ok(crate::services::download_client::SelectiveOutcome::Filtered(kept)) => Some(kept),
            Ok(crate::services::download_client::SelectiveOutcome::FullDownload) => None,
            Err(e) => {
                logger::warn(
                    &state.db,
                    LogCategory::Grab,
                    &format!(
                        "Selective download failed, falling back to full grab: {}",
                        title
                    ),
                    &e,
                )
                .await;
                qbit.add_torrent(&url, &info_hash)
                    .await
                    .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
                None
            }
        }
    } else {
        qbit.add_torrent(&url, &info_hash)
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e))?;
        None
    };

    // Interactive grab: the frontend doesn't currently round-trip the Nyaa
    // view URL, so Layer 2 is skipped here. These grabs are user-initiated
    // and rarely land on the ambiguous tail that Layer 2 targets anyway.
    // Layer 4 still runs when we have a tracked series — it's a pure
    // function with no round-trip cost.
    let series_ctx = tracked_row
        .as_ref()
        .map(|s| crate::services::source::SeriesContext {
            status: &s.status,
            season_year: s.season_year,
            end_year: s.end_year,
        });
    let classification = crate::services::source::classify_release(
        &state.db,
        &title,
        Some(&resolution),
        None,
        series_ctx,
    )
    .await;
    let selective_suffix = match (&selective_outcome, wants_selective) {
        (Some(kept), _) => format!(", selective={}", kept.len()),
        (None, true) => ", selective=full(timeout)".to_string(),
        (None, false) => String::new(),
    };
    logger::info(
        &state.db,
        LogCategory::Grab,
        &format!("Interactive grab: {}", title),
        &format!(
            "episode={}, group={}, tier={}{}",
            episode_number,
            group,
            classification.label(),
            selective_suffix
        ),
    )
    .await;

    if let Some(sid) = series_id {
        // Interactive single-episode grab — not a batch by definition.
        let _ = crate::models::grabbed_torrents::record_grab(
            &state.db,
            &info_hash,
            &title,
            sid,
            &[episode_number],
            false,
        )
        .await;
        let _ = episode_tags::record_grab(
            &state.db,
            sid,
            episode_number,
            &classification,
            &title,
            &group,
            size_bytes,
            false,
        )
        .await;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "selective_files": selective_outcome,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::grabbed_torrents;

    fn empty_anime_detail(id: i64, title_english: &str) -> anilist::AnimeDetail {
        anilist::AnimeDetail {
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes: Some(26),
            duration: Some(24),
            season: String::new(),
            season_year: Some(2012),
            end_year: Some(2013),
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn test_grab_ctx() -> AutoExpandGrabContext {
        AutoExpandGrabContext {
            classification: crate::services::source::ClassificationResult::unknown(),
            release_group: String::new(),
            size_bytes: 0,
        }
    }

    fn related_entry(id: i64, title_english: &str, episodes: Option<i32>) -> anilist::RelatedEntry {
        anilist::RelatedEntry {
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes,
            relation_type: "SIDE_STORY".to_string(),
            season_year: Some(2014),
            media_type: "ANIME".to_string(),
        }
    }

    /// End-to-end exercise of the Phase 2 auto-expand route writer.
    /// Mirrors the real JoJo S1-S3 megapack case: the parent entry
    /// ("JoJo's Bizarre Adventure") owns the Phantom Blood / Battle
    /// Tendency files and a sibling relation ("Stardust Crusaders")
    /// owns the S3 files. After the pure inner fn runs, we expect two
    /// route rows to land in `grabbed_torrent_series` — one per series
    /// — with the unclaimed (parent) files routing to the franchise
    /// root.
    ///
    /// The fn is split into outer (qBit metadata wait) + inner
    /// (`_with_files`) precisely so this test can feed synthetic
    /// filenames without spinning up qBittorrent.
    #[tokio::test]
    async fn auto_expand_routes_sibling_and_parent_files() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        // Seed the parent series row. The real grab path calls
        // series::upsert first, so this matches production flow.
        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 801,
                mal_id: None,
                title: "JoJo's Bizarre Adventure",
                title_romaji: "JoJo no Kimyou na Bouken",
                title_english: "JoJo's Bizarre Adventure",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(26),
                season_year: Some(2012),
                end_year: Some(2013),
            },
        )
        .await
        .expect("parent upsert");

        // Record a grab row so there's a grab_id for the routes to
        // attach to. `record_grab` returns Ok(Some(id)) on fresh insert.
        let grab_id = grabbed_torrents::record_grab(
            &db,
            "dummyhash0000000000000000000000000000000",
            "[Group] JoJo Megapack (BD 1080p)",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab row inserted");

        // Construct a parent AnimeDetail with one sibling relation.
        // The sibling title must carry an extractable trailing
        // subtitle ("Stardust Crusaders") for detect_sibling_entries
        // to find a needle to match.
        let mut parent_detail = empty_anime_detail(801, "JoJo's Bizarre Adventure");
        parent_detail.relations.push(related_entry(
            802,
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            Some(24),
        ));

        // Two sibling files (match the Stardust Crusaders needle) and
        // two parent files (bare franchise title, no sibling subtitle).
        let filenames = vec![
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01 [BD 1080p].mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 02 [BD 1080p].mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - 01 [BD 1080p].mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - 02 [BD 1080p].mkv".to_string(),
        ];

        let grab_ctx = test_grab_ctx();
        let added = expand_from_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &[1, 2],
            grab_id,
            "[Group] JoJo Megapack (BD 1080p)",
            &grab_ctx,
        )
        .await;

        assert_eq!(added, 1, "one new sibling (Stardust Crusaders) expected");

        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert_eq!(routes.len(), 2, "sibling route + parent route expected");

        // The sibling route: claims file indices 0 and 1, its series_id
        // differs from the parent, and the matched subtitle is the one
        // trailing_subtitle_of extracted from the relation title.
        let sibling_route = routes
            .iter()
            .find(|r| r.series_id != parent_id)
            .expect("sibling route present");
        assert_eq!(sibling_route.file_indices, vec![0, 1]);
        assert_eq!(sibling_route.matched_subtitle, "Stardust Crusaders");
        // Arc-local numbering (files E01, E02) → min_ep=1 ≤
        // parent_cap=26 → offset=0, and stored episode_numbers
        // equal the raw parsed values.
        assert_eq!(sibling_route.episode_offset, 0);
        assert_eq!(sibling_route.episode_numbers, vec![1, 2]);

        // The parent route: claims the unclaimed media files (2 and 3)
        // and reuses the caller-supplied episode numbers verbatim.
        let parent_route = routes
            .iter()
            .find(|r| r.series_id == parent_id)
            .expect("parent route present");
        assert_eq!(parent_route.file_indices, vec![2, 3]);
        assert_eq!(parent_route.episode_numbers, vec![1, 2]);
        // Parent routes always carry offset 0.
        assert_eq!(parent_route.episode_offset, 0);
    }

    /// Smol Monogatari-style batch: absolute episode numbering runs
    /// across parent + sibling (E13 = last parent ep, E14 = first
    /// sibling ep). The fallback path detects Owarimonogatari Second
    /// Season via title-prefix matching AND the per-sibling offset
    /// pass sets offset=13 so the route row's episode_numbers store
    /// the effective (arc-local) 1..=7 instead of the raw 14..=20.
    #[tokio::test]
    async fn auto_expand_persists_episode_offset_for_absolute_numbered_batch() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21320,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(13),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("parent upsert");

        let grab_id = grabbed_torrents::record_grab(
            &db,
            "owarismolhash00000000000000000000000000",
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab inserted");

        // Parent AnimeDetail with a continuation relation. Use the
        // real title "Owarimonogatari Second Season" — no delimiter,
        // no 2-token trailing subtitle — so the subtitle path cannot
        // match and the fallback path's title-prefix rule must fire.
        let mut parent_detail = empty_anime_detail(21320, "Owarimonogatari");
        parent_detail.episodes = Some(13);
        parent_detail.relations.push(related_entry(
            21860,
            "Owarimonogatari Second Season",
            Some(7),
        ));

        // 13 parent files (S07E01..E13) + 7 sibling files (S07E14..E20).
        let mut filenames: Vec<String> = Vec::new();
        for n in 1..=13 {
            filenames.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p).mkv",
                n
            ));
        }
        for n in 14..=20 {
            filenames.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p).mkv",
                n
            ));
        }
        let parent_episode_numbers: Vec<i32> = (1..=13).collect();

        let grab_ctx = test_grab_ctx();
        let added = expand_from_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &parent_episode_numbers,
            grab_id,
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            &grab_ctx,
        )
        .await;

        assert_eq!(added, 1, "one new sibling (Owari S2) expected");

        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert_eq!(routes.len(), 2, "sibling route + parent route expected");

        let sibling_route = routes
            .iter()
            .find(|r| r.series_id != parent_id)
            .expect("sibling route present");
        // Files 13..=19 (0-based indices) correspond to S07E14..E20.
        assert_eq!(sibling_route.file_indices, vec![13, 14, 15, 16, 17, 18, 19]);
        // The matched subtitle records the detection method for
        // operator inspection.
        assert!(
            sibling_route
                .matched_subtitle
                .starts_with("episode-range fallback")
        );
        // Absolute numbering → offset = parent_cap = 13.
        assert_eq!(sibling_route.episode_offset, 13);
        // Stored episode_numbers are effective (post-offset) values,
        // so a later `find_imported_for_episode(sibling, 1)` upgrade
        // query hits this route row correctly.
        assert_eq!(sibling_route.episode_numbers, vec![1, 2, 3, 4, 5, 6, 7]);

        let parent_route = routes
            .iter()
            .find(|r| r.series_id == parent_id)
            .expect("parent route present");
        assert_eq!(
            parent_route.file_indices,
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert_eq!(parent_route.episode_offset, 0);

        // Regression guard: auto-expand must also write per-episode
        // `episode_quality_tags` + `episode_grab_history` rows for the
        // newly-upserted sibling. Without these the sibling's series
        // page renders UNKNOWN with no progress bar until post-
        // processing backfills them (which, if the user has PP
        // disabled, never happens). Uses the effective (post-offset)
        // local episode numbers the route already stores.
        let sibling_id = sibling_route.series_id;
        let sibling_tags = episode_tags::get_for_series(&db, sibling_id)
            .await
            .expect("sibling quality tags");
        assert_eq!(
            sibling_tags.len(),
            7,
            "sibling should have 7 quality-tag rows (one per local ep 1..=7)"
        );
        for local_ep in 1..=7 {
            let tag = sibling_tags
                .get(&local_ep)
                .unwrap_or_else(|| panic!("sibling tag for local ep {} missing", local_ep));
            assert_eq!(tag.state, "grabbed");
            let history = episode_tags::get_grab_history(&db, sibling_id, local_ep)
                .await
                .expect("sibling grab history");
            assert_eq!(
                history.len(),
                1,
                "sibling local ep {} should have 1 grab-history row",
                local_ep
            );
        }
    }

    /// When the file list has no sibling matches, the inner fn is a
    /// no-op: no sibling series get upserted and no route rows get
    /// written. This exercises the early-return after
    /// `detect_sibling_entries_in_pack` returns an empty vec — the
    /// production path relies on that branch to avoid polluting the
    /// library with ghost rows for regular single-series grabs.
    #[tokio::test]
    async fn auto_expand_noop_when_no_siblings_detected() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 901,
                mal_id: None,
                title: "Sono Bisque Doll wa Koi wo Suru",
                title_romaji: "Sono Bisque Doll wa Koi wo Suru",
                title_english: "My Dress-Up Darling",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2022),
                end_year: Some(2022),
            },
        )
        .await
        .expect("parent upsert");

        let grab_id = grabbed_torrents::record_grab(
            &db,
            "dummyhash1111111111111111111111111111111",
            "[Group] My Dress-Up Darling S01 (BD 1080p)",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab row inserted");

        // No relations on the parent detail → no sibling candidates
        // even though the file list is full of media files.
        let parent_detail = empty_anime_detail(901, "My Dress-Up Darling");
        let filenames = vec![
            "[Group] My Dress-Up Darling - 01 [BD 1080p].mkv".to_string(),
            "[Group] My Dress-Up Darling - 02 [BD 1080p].mkv".to_string(),
        ];

        let grab_ctx = test_grab_ctx();
        let added = expand_from_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &[1, 2],
            grab_id,
            "[Group] My Dress-Up Darling S01 (BD 1080p)",
            &grab_ctx,
        )
        .await;

        assert_eq!(added, 0);
        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert!(
            routes.is_empty(),
            "no sibling → no routes, post-processing falls back to grab.series_id"
        );
    }

    /// #26 — Grab-time hydration gate must NOT fire when a series'
    /// only PREQUEL is a movie (format = "MOVIE"). JJK S1's only
    /// AL prequel is the JJK 0 movie; since absolute-numbered TV
    /// releases don't count movies, the existing cumulative = 0 is
    /// correct and we must not trigger an AL refresh on every
    /// auto-search.
    ///
    /// Verifies the gate returns the series unchanged (cumulative
    /// still 0) WITHOUT attempting network I/O — if the gate were
    /// wrong, refresh_series_metadata would be called and the test
    /// would hit AL (and fail with a flaky network error).
    #[tokio::test]
    async fn cumulative_hydration_skips_movie_only_prequel() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 113415,
                mal_id: None,
                title: "Jujutsu Kaisen",
                title_romaji: "Jujutsu Kaisen",
                title_english: "Jujutsu Kaisen",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(24),
                season_year: Some(2020),
                end_year: Some(2021),
            },
        )
        .await
        .expect("upsert");

        let tracked = series::get_by_id(&db, series_id)
            .await
            .expect("get_by_id")
            .expect("series exists");
        assert_eq!(tracked.cumulative_prior_episodes, 0);

        let mut detail = empty_anime_detail(113415, "Jujutsu Kaisen");
        let mut jjk0 = related_entry(145064, "Jujutsu Kaisen 0", None);
        jjk0.relation_type = "PREQUEL".to_string();
        jjk0.format = "MOVIE".to_string();
        detail.relations.push(jjk0);

        let result = maybe_hydrate_cumulative_offset(&db, Some(tracked), &detail).await;
        let after = result.expect("series still returned");
        assert_eq!(
            after.cumulative_prior_episodes, 0,
            "movie-only prequel must not trigger hydration"
        );
    }

    /// #26 — Gate must short-circuit when cumulative is already
    /// non-zero: a series that's been hydrated before (e.g. by the
    /// periodic metadata_refresh sweep) should not re-hydrate on
    /// every auto-search.
    #[tokio::test]
    async fn cumulative_hydration_skips_already_populated_series() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 145064,
                mal_id: None,
                title: "Jujutsu Kaisen S2",
                title_romaji: "Jujutsu Kaisen S2",
                title_english: "Jujutsu Kaisen S2",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(23),
                season_year: Some(2023),
                end_year: Some(2023),
            },
        )
        .await
        .expect("upsert");
        series::update_cumulative_prior_episodes(&db, series_id, 24)
            .await
            .expect("set cumulative");

        let tracked = series::get_by_id(&db, series_id)
            .await
            .expect("get_by_id")
            .expect("series exists");
        assert_eq!(tracked.cumulative_prior_episodes, 24);

        let mut detail = empty_anime_detail(145064, "Jujutsu Kaisen S2");
        let mut prev = related_entry(113415, "Jujutsu Kaisen", Some(24));
        prev.relation_type = "PREQUEL".to_string();
        prev.format = "TV".to_string();
        detail.relations.push(prev);

        let result = maybe_hydrate_cumulative_offset(&db, Some(tracked), &detail).await;
        let after = result.expect("series still returned");
        assert_eq!(
            after.cumulative_prior_episodes, 24,
            "populated cumulative short-circuits the gate"
        );
    }

    /// Issue #45: full-scale JoJo Part 3 case. 48-episode BD megapack
    /// with absolute continuous numbering (no per-cour arc markers in
    /// the filenames) and Egypt-hen as a sibling of Stardust Crusaders
    /// on AniList. Egypt-hen's trailing "subtitle" is a single token
    /// ("Egypt-hen") so the subtitle path can't match — the
    /// episode-range fallback picks it up via title-prefix matching.
    ///
    /// Verifies that:
    ///   1. detection fires once (Egypt-hen sibling).
    ///   2. the sibling route carries files 24..=47 (0-based) = E25..E48.
    ///   3. episode_offset = 24 (parent_cap) so those map to local 1..=24.
    ///   4. the sibling gets 24 quality-tag + grab-history rows (not 0).
    ///   5. the parent route carries files 0..=23 = E01..=E24, offset 0.
    #[tokio::test]
    async fn auto_expand_jojo_part3_48ep_pack_maps_all_episodes() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        // Parent: JoJo Stardust Crusaders (24 eps).
        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 20899,
                mal_id: None,
                title: "JoJo's Bizarre Adventure: Stardust Crusaders",
                title_romaji: "JoJo no Kimyou na Bouken: Stardust Crusaders",
                title_english: "JoJo's Bizarre Adventure: Stardust Crusaders",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(24),
                season_year: Some(2014),
                end_year: Some(2014),
            },
        )
        .await
        .expect("parent upsert");

        let grab_id = grabbed_torrents::record_grab(
            &db,
            "jojop3hash00000000000000000000000000000000",
            "[Group] JoJo's Bizarre Adventure Part 3 - Stardust Crusaders (BD 1080p 48 ep)",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab inserted");

        // Parent AnimeDetail with Egypt-hen as a sibling relation. Use
        // the real AL title form "... Stardust Crusaders - Egypt-hen";
        // the trailing single-token subtitle can't be extracted, so
        // detection falls through to the episode-range + title-prefix
        // path.
        let mut parent_detail =
            empty_anime_detail(20899, "JoJo's Bizarre Adventure: Stardust Crusaders");
        parent_detail.episodes = Some(24);
        parent_detail.relations.push(related_entry(
            22663,
            "JoJo's Bizarre Adventure: Stardust Crusaders - Egypt-hen",
            Some(24),
        ));

        // 48 absolute-numbered files (E01..E48).
        let filenames: Vec<String> = (1..=48)
            .map(|n| {
                format!(
                    "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - {:02} [BD 1080p].mkv",
                    n
                )
            })
            .collect();

        let grab_ctx = test_grab_ctx();
        let added = expand_from_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &(1..=48).collect::<Vec<_>>(),
            grab_id,
            "[Group] JoJo P3 BD 48ep",
            &grab_ctx,
        )
        .await;

        assert_eq!(added, 1, "one new sibling (Egypt-hen) expected");

        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert_eq!(routes.len(), 2, "sibling + parent route expected");

        let sibling_route = routes
            .iter()
            .find(|r| r.series_id != parent_id)
            .expect("sibling route present");
        let expected_sibling_files: Vec<usize> = (24..=47).collect();
        assert_eq!(
            sibling_route.file_indices, expected_sibling_files,
            "sibling owns files 24..=47 (E25..E48)"
        );
        assert_eq!(
            sibling_route.episode_offset, 24,
            "absolute numbering → offset = parent_cap = 24"
        );
        assert_eq!(
            sibling_route.episode_numbers,
            (1..=24).collect::<Vec<_>>(),
            "sibling's stored ep_nums are effective (post-offset) 1..=24"
        );

        let parent_route = routes
            .iter()
            .find(|r| r.series_id == parent_id)
            .expect("parent route present");
        let expected_parent_files: Vec<usize> = (0..=23).collect();
        assert_eq!(parent_route.file_indices, expected_parent_files);
        assert_eq!(parent_route.episode_offset, 0);

        // Sibling quality-tag + history rows exist for local 1..=24.
        let sibling_id = sibling_route.series_id;
        let sibling_tags = episode_tags::get_for_series(&db, sibling_id)
            .await
            .expect("sibling quality tags");
        assert_eq!(
            sibling_tags.len(),
            24,
            "sibling should have 24 quality-tag rows"
        );
        for local_ep in 1..=24 {
            let tag = sibling_tags
                .get(&local_ep)
                .unwrap_or_else(|| panic!("sibling tag for local ep {} missing", local_ep));
            assert_eq!(tag.state, "grabbed");
        }
    }

    /// Issue #45: Owarimonogatari BD with an AL/BD episode-count
    /// disagreement. AL reports S1 = 12 eps (the aired ep 1 was a
    /// 48-min merged episode) but the [smol] BD splits that back into
    /// two ~24-min files, so the pack has 13 Owari S1 files + 7 Owari
    /// S2 files.
    ///
    /// This is the case the user flagged as "frustrating" in issue #45.
    /// Verifies that:
    ///   1. the sibling side is unaffected by the mismatch — Owari S2
    ///      gets 7 files mapped to local 1..=7 via offset 13.
    ///   2. the parent side gets all 13 files (including the "extra"
    ///      ep 13 that AL doesn't know about), routed with offset 0.
    ///
    /// The complementary UI fix lives in `pages::build_episodes` — see
    /// `build_episodes_surfaces_on_disk_files_beyond_anilist_episode_count`.
    #[tokio::test]
    async fn auto_expand_owari_bd_split_with_anilist_count_mismatch() {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        // Parent: Owarimonogatari. Use AL's reported count (12), NOT
        // the BD's file count (13). This is the whole point of the test.
        let (parent_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21860,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("parent upsert");

        let grab_id = grabbed_torrents::record_grab(
            &db,
            "owarialmismatch000000000000000000000000000",
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            parent_id,
            &[],
            true,
        )
        .await
        .expect("record_grab")
        .expect("grab inserted");

        let mut parent_detail = empty_anime_detail(21860, "Owarimonogatari");
        parent_detail.episodes = Some(12); // AL's count, NOT the BD's.
        parent_detail.relations.push(related_entry(
            99423,
            "Owarimonogatari Second Season",
            Some(7),
        ));

        // 13 parent files (S07E01..E13) + 7 sibling files (S07E14..E20).
        // The parent's file 12 (E13) exists despite AL saying S1 has
        // only 12 episodes — this is the mismatch we're testing.
        let mut filenames: Vec<String> = Vec::new();
        for n in 1..=13 {
            filenames.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p).mkv",
                n
            ));
        }
        for n in 14..=20 {
            filenames.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p).mkv",
                n
            ));
        }

        let grab_ctx = test_grab_ctx();
        let added = expand_from_files(
            &db,
            &filenames,
            &parent_detail,
            parent_id,
            &(1..=12).collect::<Vec<_>>(),
            grab_id,
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            &grab_ctx,
        )
        .await;

        assert_eq!(added, 1, "one new sibling (Owari S2) expected");

        let routes = grabbed_torrents::get_series_routes(&db, grab_id)
            .await
            .expect("get_series_routes");
        assert_eq!(routes.len(), 2, "sibling + parent route expected");

        // Sibling route: 7 files (S07E14..E20) mapped to local 1..=7.
        // `min_ep = 14`, parent_cap = 12 → offset = min_ep - 1 = 13.
        // Local = raw - offset: 14→1, 15→2, ..., 20→7.
        let sibling_route = routes
            .iter()
            .find(|r| r.series_id != parent_id)
            .expect("sibling route present");
        assert_eq!(
            sibling_route.file_indices,
            vec![13, 14, 15, 16, 17, 18, 19],
            "sibling owns E14..=E20 (0-based 13..=19)"
        );
        assert_eq!(
            sibling_route.episode_offset, 13,
            "min_ep(14) - 1 = 13, correctly larger than parent_cap(12)"
        );
        assert_eq!(sibling_route.episode_numbers, vec![1, 2, 3, 4, 5, 6, 7]);

        // Parent route: all 13 files including the "extra" E13 that
        // AL doesn't know about. Offset stays 0 — parent files use
        // their own local numbering.
        let parent_route = routes
            .iter()
            .find(|r| r.series_id == parent_id)
            .expect("parent route present");
        assert_eq!(
            parent_route.file_indices,
            (0..=12).collect::<Vec<_>>(),
            "parent owns all 13 files (E01..=E13), including the AL-overflow E13"
        );
        assert_eq!(parent_route.episode_offset, 0);
    }
}
