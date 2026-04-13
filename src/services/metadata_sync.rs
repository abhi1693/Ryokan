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
    if provider_id > 0 && !force_mal_fallback {
        if let Ok(detail) = anilist::get_anime_detail_with_options(provider_id, mal_id, false).await {
            return Ok(detail);
        }
    }

    if let Some(mal_id) = mal_id {
        if let Ok(detail) = jikan::get_anime_detail_cached(mal_id).await {
            return Ok(detail);
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
    if root_provider_id == 0 {
        return;
    }

    let mut seen: HashSet<i64> = HashSet::new();
    let mut queue: VecDeque<(i64, Option<i64>)> = VecDeque::new();
    queue.push_back((root_provider_id, root_detail.id_mal));
    let mut processed = 0usize;

    while let Some((provider_id, mal_id)) = queue.pop_front() {
        if provider_id == 0 || !seen.insert(provider_id) {
            continue;
        }
        if processed >= MAX_RELATION_TREE_NODES {
            break;
        }
        processed += 1;

        let detail = if provider_id == root_provider_id {
            root_detail.clone()
        } else {
            match fetch_live_detail_for_ids(provider_id, mal_id, &Vec::new(), None, force_mal_fallback).await {
                Ok(detail) => detail,
                Err(_) => continue,
            }
        };

        let _ = cache_provider_detail(db, provider_id, &detail, force_kitsu_fallback).await;

        for related in &detail.relations {
            if related.id != 0 && matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
                queue.push_back((related.id, related.id_mal));
            }
        }
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

pub async fn rebuild_cached_metadata(
    db: &SqlitePool,
    tracked: &series::Series,
    force_mal_fallback: bool,
) -> Result<anilist::AnimeDetail, String> {
    refresh_series_metadata_inner(db, tracked, force_mal_fallback, true).await
}

pub async fn rebuild_cached_metadata_for_all(db: &SqlitePool) -> (usize, usize, usize) {
    let tracked = match series::get_all(db).await {
        Ok(items) => items,
        Err(err) => {
            logger::error(db, LogCategory::AniList, "Cached metadata rebuild sweep failed", &err.to_string()).await;
            return (0, 0, 1);
        }
    };

    let force_mal_fallback = crate::models::config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.force_mal_fallback)
        .unwrap_or(false);

    let mut rebuilt = 0usize;
    let skipped = 0usize;
    let mut failed = 0usize;

    for tracked in tracked {
        match rebuild_cached_metadata(db, &tracked, force_mal_fallback).await {
            Ok(detail) => {
                rebuilt += 1;
                logger::info(
                    db,
                    LogCategory::AniList,
                    &format!("Rebuilt cached metadata for {}", tracked.title),
                    &format!("provider_id={}, anilist_id={}, mal_id={:?}, episodes={:?}", detail.id, tracked.anilist_id, detail.id_mal, detail.episodes),
                ).await;
            }
            Err(err) => {
                failed += 1;
                logger::warn(
                    db,
                    LogCategory::AniList,
                    &format!("Failed to rebuild cached metadata for {}", tracked.title),
                    &err,
                ).await;
            }
        }
    }

    logger::info(
        db,
        LogCategory::AniList,
        "Cached metadata rebuild sweep complete",
        &format!("rebuilt={}, skipped={}, failed={}", rebuilt, skipped, failed),
    ).await;

    (rebuilt, skipped, failed)
}

pub async fn refresh_all_series_metadata(db: &SqlitePool) -> (usize, usize) {
    let tracked = match series::get_all(db).await {
        Ok(items) => items,
        Err(err) => {
            logger::error(db, LogCategory::AniList, "Metadata refresh sweep failed", &err.to_string()).await;
            return (0, 1);
        }
    };

    let force_mal_fallback = crate::models::config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.force_mal_fallback)
        .unwrap_or(false);

    let mut refreshed = 0usize;
    let mut failed = 0usize;
    for tracked in tracked {
        match refresh_series_metadata(db, &tracked, force_mal_fallback).await {
            Ok(_) => refreshed += 1,
            Err(err) => {
                failed += 1;
                logger::warn(
                    db,
                    LogCategory::AniList,
                    &format!("Metadata refresh failed for {}", tracked.title),
                    &err,
                ).await;
            }
        }
    }

    if refreshed > 0 || failed > 0 {
        logger::info(
            db,
            LogCategory::AniList,
            "Metadata refresh sweep complete",
            &format!("refreshed={}, failed={}", refreshed, failed),
        ).await;
    }

    (refreshed, failed)
}
