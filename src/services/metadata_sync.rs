use std::collections::{HashMap, HashSet, VecDeque};

use sqlx::SqlitePool;

use crate::models::{config, local_metadata, metadata_cache, series};
use crate::models::log::LogCategory;
use crate::services::{anilist, artwork, jikan, kitsu, logger};

const MAX_RELATION_TREE_NODES: usize = 64;

fn is_authoritative_detail(tracked: &series::Series, detail: &anilist::AnimeDetail) -> bool {
    if tracked.anilist_id <= 0 {
        return true;
    }
    detail.id > 0 && detail.id == tracked.anilist_id
}

fn title_candidates_for_series(tracked: &series::Series) -> Vec<String> {
    let mut titles = vec![
        tracked.title.clone(),
        tracked.title_romaji.clone(),
        tracked.title_english.clone(),
        tracked.title_native.clone(),
    ];
    titles.retain(|t| !t.trim().is_empty());
    titles
}

async fn fetch_live_detail(
    tracked: &series::Series,
    force_mal_fallback: bool,
) -> Result<anilist::AnimeDetail, String> {
    fetch_live_detail_for_ids(
        tracked.anilist_id,
        tracked.mal_id,
        &title_candidates_for_series(tracked),
        tracked.episodes,
        force_mal_fallback,
    )
    .await
}

async fn fetch_live_detail_for_ids(
    provider_id: i64,
    mal_id: Option<i64>,
    title_candidates: &[String],
    episode_count: Option<i32>,
    force_mal_fallback: bool,
) -> Result<anilist::AnimeDetail, String> {
    // Fallback policy: when an entry has a real AniList ID and the user
    // hasn't explicitly opted into MAL, MAL is only used when AL is
    // genuinely *down* (5xx, network error, parse failure, etc.). A 429
    // rate-limit means AL is responding but throttling us — we surface
    // an Err so the caller can defer-and-retry, preserving AL fidelity
    // instead of silently substituting MAL data. Persistent rate-limits
    // are the user's own library not getting fully refreshed; we'd
    // rather leave the previous AL data in place than overwrite it with
    // a MAL approximation.
    if provider_id > 0 && !force_mal_fallback {
        if anilist::anilist_cooldown_active() {
            return Err(format!(
                "AniList rate-limit cooldown active for provider_id={} \
                 (no MAL/Kitsu fallback)",
                provider_id
            ));
        }
        match anilist::get_anime_detail_with_options(provider_id, mal_id, false).await {
            Ok(detail) => return Ok(detail),
            Err(err) => {
                if anilist::is_rate_limit_error(&err) {
                    tracing::warn!(
                        target: "ryokan::metadata_sync",
                        provider_id,
                        mal_id = ?mal_id,
                        error = %err,
                        "AniList rate-limited (no MAL/Kitsu fallback)"
                    );
                    return Err(err);
                }
                tracing::warn!(
                    target: "ryokan::metadata_sync",
                    provider_id,
                    mal_id = ?mal_id,
                    error = %err,
                    "AniList detail fetch failed; falling back to MAL/Kitsu"
                );
            }
        }
    }

    if let Some(mid) = mal_id {
        match jikan::get_anime_detail_cached(mid).await {
            Ok(detail) => return Ok(detail),
            Err(err) => {
                tracing::warn!(
                    target: "ryokan::metadata_sync",
                    mal_id = mid,
                    error = %err,
                    "Jikan/MAL detail fetch failed; falling back to Kitsu by title"
                );
            }
        }
    }

    if !title_candidates.is_empty() {
        return kitsu::get_anime_detail_by_titles(title_candidates, None, episode_count).await;
    }

    anilist::get_anime_detail_with_options(provider_id, mal_id, force_mal_fallback).await
}


fn episode_needs_kitsu_backfill<F>(ep_count: i32, mut has_jikan_title: F) -> bool
where
    F: FnMut(i32) -> bool,
{
    if ep_count <= 1 {
        return false;
    }

    (1..=ep_count).any(|ep_num| !has_jikan_title(ep_num))
}

async fn build_episode_cache(
    db: &SqlitePool,
    detail: &anilist::AnimeDetail,
    force_kitsu_fallback: bool,
) -> Vec<local_metadata::CachedEpisodeMetadata> {
    // Use the effective count so airing series (episodes=null on AniList)
    // still get an episode cache built from `nextAiringEpisode - 1`. Without
    // this, shows like One Piece end up with zero rows in
    // `series_episode_metadata`, which in turn leaves `episode_monitor_state`
    // empty and breaks the monitoring UI.
    let ep_count = detail.effective_episode_count();
    let episodic_format = !matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA");
    let should_fetch_jikan = episodic_format || ep_count > 1;

    let mut jikan_eps = if should_fetch_jikan {
        jikan::fetch_episode_titles_for_detail(db, detail).await
    } else {
        HashMap::new()
    };

    let kitsu_titles = vec![
        detail.title_english.clone(),
        detail.title_romaji.clone(),
        detail.title_native.clone(),
    ];
    let should_try_kitsu = ep_count > 1
        && (force_kitsu_fallback || episode_needs_kitsu_backfill(ep_count, |ep_num| {
            jikan_eps
                .get(&ep_num)
                .map(|info| !info.title.trim().is_empty())
                .unwrap_or(false)
        }));

    let kitsu_eps = if should_try_kitsu {
        kitsu::fetch_episode_titles_fallback(db, &kitsu_titles, detail.season_year, Some(ep_count)).await
    } else {
        HashMap::new()
    };

    let mut merged = Vec::new();
    for ep_num in 1..=ep_count {
        let fallback_title = if ep_count <= 1 {
            if !detail.title_english.trim().is_empty() {
                detail.title_english.clone()
            } else if !detail.title_romaji.trim().is_empty() {
                detail.title_romaji.clone()
            } else {
                detail.title_native.clone()
            }
        } else {
            String::new()
        };

        let local = if force_kitsu_fallback {
            kitsu_eps
                .get(&ep_num)
                .map(|kitsu| {
                    (
                        if !kitsu.title.trim().is_empty() {
                            kitsu.title.clone()
                        } else {
                            fallback_title.clone()
                        },
                        kitsu.aired.clone(),
                        "kitsu".to_string(),
                    )
                })
                .or_else(|| {
                    jikan_eps.get(&ep_num).map(|j| {
                        (
                            if !j.title.trim().is_empty() {
                                j.title.clone()
                            } else {
                                fallback_title.clone()
                            },
                            j.aired.clone(),
                            "jikan".to_string(),
                        )
                    })
                })
        } else {
            jikan_eps
                .remove(&ep_num)
                .map(|j| {
                    (
                        if !j.title.trim().is_empty() {
                            j.title
                        } else {
                            fallback_title.clone()
                        },
                        j.aired,
                        "jikan".to_string(),
                    )
                })
                .or_else(|| {
                    kitsu_eps.get(&ep_num).map(|kitsu| {
                        (
                            if !kitsu.title.trim().is_empty() {
                                kitsu.title.clone()
                            } else {
                                fallback_title.clone()
                            },
                            kitsu.aired.clone(),
                            "kitsu".to_string(),
                        )
                    })
                })
        };

        let (title, aired, source) =
            local.unwrap_or((fallback_title.clone(), String::new(), "series".to_string()));
        merged.push(local_metadata::CachedEpisodeMetadata {
            episode_number: ep_num,
            title: title.clone(),
            title_romaji: title.clone(),
            title_english: title.clone(),
            title_native: title,
            aired,
            source,
        });
    }

    merged
}

async fn cache_provider_detail(
    db: &SqlitePool,
    cache_provider_id: i64,
    detail: &anilist::AnimeDetail,
    force_kitsu_fallback: bool,
) -> Result<(), String> {
    if cache_provider_id == 0 {
        return Ok(());
    }

    metadata_cache::upsert_provider(db, cache_provider_id, detail.id_mal, detail)
        .await
        .map_err(|e| e.to_string())?;
    local_metadata::replace_relations_for_provider(db, cache_provider_id, detail)
        .await
        .map_err(|e| e.to_string())?;
    let merged = build_episode_cache(db, detail, force_kitsu_fallback).await;
    local_metadata::replace_episode_metadata_for_provider(db, cache_provider_id, &merged)
        .await
        .map_err(|e| e.to_string())?;

    artwork::cache_provider_detail_artwork(db, cache_provider_id, detail.id_mal, detail).await;
    for related in &detail.relations {
        artwork::cache_provider_relation_artwork(db, cache_provider_id, related.id, related.id_mal, &related.cover_url).await;
    }
    Ok(())
}

async fn hydrate_relation_tree(
    db: &SqlitePool,
    root_provider_id: i64,
    root_detail: &anilist::AnimeDetail,
    force_mal_fallback: bool,
    force_kitsu_fallback: bool,
) {
    // Trees stay strictly separate. AL mode walks AL's relation graph
    // (positive AL IDs only); MAL mode walks MAL's, where Jikan stamps
    // each card with `id = -mal_id` as a "no AL mapping" sentinel. Mode
    // is determined by which provider gave us the root: positive root
    // detail id = AL, negative = MAL fallback (or user opted into MAL).
    // We never interleave the two — a MAL fallback mid-walk would
    // pollute AL's graph with MAL-only siblings (e.g. JoJo Part 6 is 3
    // entries on MAL but 2 on AL).
    //
    // Rate-limited AL relations defer-and-retry within the walk, but we
    // never substitute MAL on rate-limit; "MAL only when AL is down"
    // means non-rate-limit errors (5xx/network), which fetch_live_detail
    // already handles inline before this walker is even called.
    const MAX_AL_RETRY_ROUNDS: usize = 3;
    const COOLDOWN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    if root_provider_id == 0 {
        return;
    }

    let mal_mode = force_mal_fallback || root_detail.id < 0;

    let mut seen: HashSet<i64> = HashSet::new();
    let mut queue: VecDeque<(i64, Option<i64>)> = VecDeque::new();
    let mut deferred: VecDeque<(i64, Option<i64>)> = VecDeque::new();
    queue.push_back((root_provider_id, root_detail.id_mal));
    seen.insert(root_provider_id);
    let mut processed = 0usize;
    let mut al_round = 0usize;

    loop {
        while let Some((provider_id, mal_id)) = queue.pop_front() {
            if processed >= MAX_RELATION_TREE_NODES {
                break;
            }

            let detail = if provider_id == root_provider_id {
                root_detail.clone()
            } else {
                match fetch_live_detail_for_ids(
                    provider_id,
                    mal_id,
                    &Vec::new(),
                    None,
                    force_mal_fallback,
                )
                .await
                {
                    Ok(detail) => detail,
                    Err(err) => {
                        // Only AL rate-limits are worth retrying. MAL
                        // failures or genuine AL-down errors are already
                        // terminal by the time this returns.
                        if anilist::is_rate_limit_error(&err) && !mal_mode {
                            deferred.push_back((provider_id, mal_id));
                        }
                        continue;
                    }
                }
            };

            processed += 1;
            let _ = cache_provider_detail(db, provider_id, &detail, force_kitsu_fallback).await;

            for related in &detail.relations {
                let id_valid = if mal_mode {
                    related.id != 0
                } else {
                    related.id > 0
                };
                if id_valid
                    && matches!(related.media_type.as_str(), "ANIME" | "MUSIC")
                    && seen.insert(related.id)
                {
                    queue.push_back((related.id, related.id_mal));
                }
            }
        }

        if deferred.is_empty() || al_round >= MAX_AL_RETRY_ROUNDS {
            // Anything still deferred after the retry budget is left out
            // of this sweep's cache — the next periodic refresh picks it
            // up. We don't substitute MAL on rate-limit (would mix trees
            // and downgrade fidelity).
            if !deferred.is_empty() {
                tracing::warn!(
                    target: "ryokan::metadata_sync",
                    root_provider_id,
                    dropped = deferred.len(),
                    retry_rounds = al_round,
                    "relation hydration left {} relations unfetched after AniList \
                     retry budget exhausted; next sweep will retry",
                    deferred.len()
                );
            }
            break;
        }

        while anilist::anilist_cooldown_active() {
            tokio::time::sleep(COOLDOWN_POLL_INTERVAL).await;
        }

        queue.append(&mut deferred);
        al_round += 1;
    }
}

async fn refresh_series_metadata_inner(
    db: &SqlitePool,
    tracked: &series::Series,
    force_mal_fallback: bool,
    allow_degraded_cache_rebuild: bool,
) -> Result<anilist::AnimeDetail, String> {
    let detail = fetch_live_detail(tracked, force_mal_fallback).await?;
    let authoritative_detail = is_authoritative_detail(tracked, &detail);

    let force_kitsu_fallback = config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|cfg| cfg.force_kitsu_fallback)
        .unwrap_or(false);

    let stored_anilist_id = if authoritative_detail { detail.id } else { tracked.anilist_id };

    if authoritative_detail || allow_degraded_cache_rebuild {
        let primary_title = if !detail.title_english.trim().is_empty() {
            &detail.title_english
        } else {
            &detail.title_romaji
        };
        series::refresh_core_metadata(
            db,
            tracked.id,
            series::SeriesCore {
                anilist_id: stored_anilist_id,
                mal_id: detail.id_mal,
                title: primary_title,
                title_romaji: &detail.title_romaji,
                title_english: &detail.title_english,
                title_native: &detail.title_native,
                cover_url: &detail.cover_url,
                format: &detail.format,
                status: &detail.status,
                episodes: detail.episodes,
                season_year: detail.season_year,
                end_year: detail.end_year,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

        metadata_cache::upsert(db, tracked.id, stored_anilist_id, detail.id_mal, &detail)
            .await
            .map_err(|e| e.to_string())?;

        artwork::cache_series_detail_artwork(db, tracked.id, &detail).await;
        for related in detail.relations.iter().filter(|r| matches!(r.media_type.as_str(), "ANIME" | "MUSIC")) {
            artwork::cache_relation_artwork(db, tracked.id, related.id, related.id_mal, &related.cover_url).await;
        }

        local_metadata::replace_relations_for_series(db, tracked.id, &detail)
            .await
            .map_err(|e| e.to_string())?;

        cache_provider_detail(db, stored_anilist_id, &detail, force_kitsu_fallback).await?;
        hydrate_relation_tree(db, stored_anilist_id, &detail, force_mal_fallback, force_kitsu_fallback).await;

        if !authoritative_detail {
            logger::info(
                db,
                LogCategory::AniList,
                &format!("Rebuilt cached metadata from fallback source for {}", tracked.title),
                &format!("provider_detail_id={}, preserved_anilist_id={}, mal_id={:?}", detail.id, tracked.anilist_id, detail.id_mal),
            )
            .await;
        }
    } else {
        logger::info(
            db,
            LogCategory::AniList,
            &format!("Preserving cached AniList relations for {}", tracked.title),
            &format!("degraded provider detail id={} anilist_id={}", detail.id, tracked.anilist_id),
        )
        .await;
    }

    let merged = build_episode_cache(db, &detail, force_kitsu_fallback).await;
    local_metadata::replace_episode_metadata(db, tracked.id, &merged)
        .await
        .map_err(|e| e.to_string())?;

    Ok(detail)
}

pub async fn refresh_series_metadata(
    db: &SqlitePool,
    tracked: &series::Series,
    force_mal_fallback: bool,
) -> Result<anilist::AnimeDetail, String> {
    refresh_series_metadata_inner(db, tracked, force_mal_fallback, false).await
}

/// Shared sweep driver for the manual rebuild and the periodic refresh.
/// `rebuild_artifacts = true` runs the full rebuild path (re-derives
/// episode metadata, artwork, etc. via refresh_series_metadata_inner's
/// `rebuild` flag); `false` runs the lighter periodic refresh.
///
/// Defer-and-retry policy: when a series is rate-limited by AniList,
/// it's parked in `deferred` instead of counted as `failed`. After the
/// main pass completes, the helper waits for the AniList cooldown to
/// clear and re-runs the deferred series. This is what makes the
/// manual rebuild button do what its name promises — a sweep that hits
/// rate limiting won't leave the user with stale or substituted data
/// for half their library; it'll finish what it started.
///
/// Bounded by `MAX_RETRY_ROUNDS` so a sustained AniList outage doesn't
/// pin the sweep forever; anything still deferred at the end counts as
/// failed and the next periodic refresh will pick it up.
async fn run_metadata_sweep(db: &SqlitePool, rebuild_artifacts: bool) -> (usize, usize) {
    const MAX_RETRY_ROUNDS: usize = 3;
    const COOLDOWN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    // Inter-series spacing: AniList allows 30 req/min for anonymous
    // clients, but in practice sustained bursts trip rate limits
    // even when the per-minute average is low. A 1-second sleep
    // between iterations paces the sweep at ~50 req/min worst-case
    // (one request every 1 + call-duration seconds, where the call
    // itself takes 0.5–2s for a typical entry), which empirically
    // stays under the rate limit on a small library.
    const INTER_SERIES_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

    let sweep_label = if rebuild_artifacts {
        "Cached metadata rebuild"
    } else {
        "Metadata refresh"
    };
    let per_series_fail_label = if rebuild_artifacts {
        "Failed to rebuild cached metadata"
    } else {
        "Metadata refresh failed"
    };

    let tracked = match series::get_all(db).await {
        Ok(items) => items,
        Err(err) => {
            logger::error(
                db,
                LogCategory::AniList,
                &format!("{} sweep failed", sweep_label),
                &err.to_string(),
            )
            .await;
            return (0, 1);
        }
    };

    let force_mal_fallback = crate::models::config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.force_mal_fallback)
        .unwrap_or(false);

    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut deferred: Vec<series::Series> = Vec::new();

    let should_defer = |tracked: &series::Series| -> bool {
        anilist::anilist_cooldown_active() && tracked.anilist_id > 0 && !force_mal_fallback
    };

    // ── Main pass ────────────────────────────────────────────────────────
    let total = tracked.len();
    for (idx, tracked) in tracked.into_iter().enumerate() {
        // Pre-check: if AL cooldown is already active, don't burn the
        // call — defer immediately. (Saves a guaranteed-to-fail HTTP
        // round trip per series.)
        if should_defer(&tracked) {
            deferred.push(tracked);
            if idx + 1 < total {
                tokio::time::sleep(INTER_SERIES_DELAY).await;
            }
            continue;
        }

        match refresh_series_metadata_inner(db, &tracked, force_mal_fallback, rebuild_artifacts).await {
            Ok(detail) => {
                succeeded += 1;
                if rebuild_artifacts {
                    logger::info(
                        db,
                        LogCategory::AniList,
                        &format!("Rebuilt cached metadata for {}", tracked.title),
                        &format!(
                            "provider_id={}, anilist_id={}, mal_id={:?}, episodes={:?}",
                            detail.id, tracked.anilist_id, detail.id_mal, detail.episodes
                        ),
                    )
                    .await;
                }
            }
            Err(err) => {
                // Post-check: this call may have been the 429 that just
                // tripped the cooldown. If so, defer instead of failing
                // so the retry round picks it up.
                if should_defer(&tracked) {
                    deferred.push(tracked);
                } else {
                    failed += 1;
                    logger::warn(
                        db,
                        LogCategory::AniList,
                        &format!("{} for {}", per_series_fail_label, tracked.title),
                        &err,
                    )
                    .await;
                }
            }
        }
        if idx + 1 < total {
            tokio::time::sleep(INTER_SERIES_DELAY).await;
        }
    }

    // ── Retry rounds for deferred series ─────────────────────────────────
    let mut round = 0;
    while !deferred.is_empty() && round < MAX_RETRY_ROUNDS {
        if anilist::anilist_cooldown_active() {
            logger::info(
                db,
                LogCategory::AniList,
                &format!(
                    "{}: waiting for AniList cooldown ({} series deferred, retry round {})",
                    sweep_label,
                    deferred.len(),
                    round + 1
                ),
                "",
            )
            .await;
            while anilist::anilist_cooldown_active() {
                tokio::time::sleep(COOLDOWN_POLL_INTERVAL).await;
            }
        }

        let to_retry = std::mem::take(&mut deferred);
        let total = to_retry.len();
        for (idx, tracked) in to_retry.into_iter().enumerate() {
            match refresh_series_metadata_inner(db, &tracked, force_mal_fallback, rebuild_artifacts).await {
                Ok(detail) => {
                    succeeded += 1;
                    if rebuild_artifacts {
                        logger::info(
                            db,
                            LogCategory::AniList,
                            &format!("Rebuilt cached metadata for {}", tracked.title),
                            &format!(
                                "provider_id={}, anilist_id={}, mal_id={:?}, episodes={:?}",
                                detail.id, tracked.anilist_id, detail.id_mal, detail.episodes
                            ),
                        )
                        .await;
                    }
                }
                Err(err) => {
                    if should_defer(&tracked) {
                        deferred.push(tracked);
                    } else {
                        failed += 1;
                        logger::warn(
                            db,
                            LogCategory::AniList,
                            &format!("{} for {}", per_series_fail_label, tracked.title),
                            &err,
                        )
                        .await;
                    }
                }
            }
            if idx + 1 < total {
                tokio::time::sleep(INTER_SERIES_DELAY).await;
            }
        }
        round += 1;
    }

    // Anything still deferred after MAX_RETRY_ROUNDS counts as failed —
    // at that point AniList is sustainedly unavailable and we should
    // surface that rather than spin. Next periodic refresh will pick it up.
    if !deferred.is_empty() {
        failed += deferred.len();
        for tracked in &deferred {
            logger::warn(
                db,
                LogCategory::AniList,
                &format!(
                    "{} skipped after {} retry rounds: {}",
                    sweep_label, MAX_RETRY_ROUNDS, tracked.title
                ),
                "AniList still rate-limited; will retry on next sweep",
            )
            .await;
        }
    }

    // Match the previous summary-log behaviour: rebuild always logs;
    // refresh only logs when something happened.
    if rebuild_artifacts || succeeded > 0 || failed > 0 {
        let detail = if rebuild_artifacts {
            format!("rebuilt={}, skipped=0, failed={}", succeeded, failed)
        } else {
            format!("refreshed={}, failed={}", succeeded, failed)
        };
        logger::info(
            db,
            LogCategory::AniList,
            &format!("{} sweep complete", sweep_label),
            &detail,
        )
        .await;
    }

    (succeeded, failed)
}

pub async fn rebuild_cached_metadata_for_all(db: &SqlitePool) -> (usize, usize, usize) {
    let (rebuilt, failed) = run_metadata_sweep(db, true).await;
    // The middle "skipped" counter has been zero for a while — the
    // sweep doesn't have a "skip without trying" branch — but the
    // tuple shape is part of the handler contract so keep it.
    (rebuilt, 0, failed)
}

pub async fn refresh_all_series_metadata(db: &SqlitePool) -> (usize, usize) {
    run_metadata_sweep(db, false).await
}
