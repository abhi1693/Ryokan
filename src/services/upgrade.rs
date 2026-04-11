use std::collections::{HashMap, HashSet};

use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, metadata_cache, monitoring, series};
use crate::services::{auto_search, logger, media, quality};
use crate::AppState;

static UPGRADE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

pub struct UpgradeSummary {
    pub series_checked: usize,
    pub episodes_checked: usize,
    pub upgrades_grabbed: usize,
    pub detail: String,
}

pub async fn run_once(state: &AppState) -> Result<UpgradeSummary, String> {
    let _guard = UPGRADE_LOCK
        .try_lock()
        .map_err(|_| "Upgrade search is already running".to_string())?;

    let cfg = config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    let cutoff_tier = quality::QualityTier::from_str(&cfg.quality_cutoff);
    if cutoff_tier == quality::QualityTier::Unknown {
        return Ok(UpgradeSummary {
            series_checked: 0,
            episodes_checked: 0,
            upgrades_grabbed: 0,
            detail: "No quality cutoff configured; skipping upgrade search".to_string(),
        });
    }

    let tracked = series::get_all(&state.db)
        .await
        .map_err(|e| e.to_string())?;

    let qbit = state.qbit.read().await.clone();
    let Some(client) = qbit.as_ref() else {
        return Ok(UpgradeSummary {
            series_checked: 0,
            episodes_checked: 0,
            upgrades_grabbed: 0,
            detail: "qBittorrent not configured; skipping upgrade search".to_string(),
        });
    };

    let mut total_series_checked: usize = 0;
    let mut total_episodes_checked: usize = 0;
    let mut total_upgrades_grabbed: usize = 0;

    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        "Upgrade search started",
        &format!("{} tracked series", tracked.len()),
    )
    .await;

    for row in &tracked {
        // Skip series with no folder (not set up yet).
        if row.folder_name.is_empty() {
            continue;
        }

        let disk_files = media::scan_series_folder(&cfg.media_root, &row.folder_name);
        if disk_files.is_empty() {
            continue;
        }

        let monitored_eps = monitoring::get_monitored_episode_numbers(&state.db, row.id)
            .await
            .unwrap_or_default();
        if monitored_eps.is_empty() {
            continue;
        }

        let quality_tags = episode_tags::get_for_series(&state.db, row.id)
            .await
            .unwrap_or_default();

        // Only consider episodes that are actually on disk — skip missing ones.
        let on_disk_eps: HashSet<i32> = disk_files.iter().map(|f| f.episode_number).collect();
        let monitored_on_disk: Vec<i32> = monitored_eps
            .iter()
            .copied()
            .filter(|ep| on_disk_eps.contains(ep))
            .collect();

        let upgrade_targets =
            auto_search::build_upgrade_targets(&disk_files, &monitored_on_disk, cutoff_tier, &quality_tags);
        if upgrade_targets.is_empty() {
            continue;
        }

        // We need an AnimeDetail for find_best_for_target. Use the metadata cache
        // to avoid hitting external APIs during background tasks.
        let detail = match metadata_cache::get_by_series_id(&state.db, row.id).await {
            Ok(Some(cached)) => cached.detail,
            _ => {
                logger::debug(
                    &state.db,
                    LogCategory::AutoSearch,
                    &format!("Upgrade: skipping {} — no cached metadata", row.title),
                    "",
                )
                .await;
                continue;
            }
        };

        total_series_checked += 1;
        let title = if !detail.title_english.is_empty() {
            &detail.title_english
        } else {
            &detail.title_romaji
        };

        let upgrade_tiers: HashMap<i32, quality::QualityTier> = upgrade_targets
            .iter()
            .filter_map(|(t, tier)| match t {
                auto_search::SearchTarget::Episode(n) => Some((*n, *tier)),
                _ => None,
            })
            .collect();

        let targets: Vec<auto_search::SearchTarget> = upgrade_targets
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let target_count = targets.len();
        total_episodes_checked += target_count;

        logger::debug(
            &state.db,
            LogCategory::AutoSearch,
            &format!("Upgrade: checking {} ({} episodes)", title, target_count),
            "",
        )
        .await;

        for target in targets {
            let label = auto_search::target_label(&target);
            // batch_episode_match=true so BD season packs can match episode targets.
            let best =
                auto_search::find_best_for_target(&detail, &cfg, &target, true, true).await;

            let Some(result) = best else {
                continue;
            };

            // Verify this is actually an upgrade.
            if let auto_search::SearchTarget::Episode(ep_num) = &target {
                if let Some(existing_tier) = upgrade_tiers.get(ep_num) {
                    let incoming_tier =
                        quality::detect_tier(&result.title, &result.resolution);
                    if incoming_tier.rank() <= existing_tier.rank() {
                        continue;
                    }
                    logger::info(
                        &state.db,
                        LogCategory::AutoSearch,
                        &format!(
                            "Upgrade: {} {} — {} -> {}",
                            title,
                            label,
                            existing_tier.label(),
                            incoming_tier.label()
                        ),
                        &result.title,
                    )
                    .await;
                }
            }

            let url = if !result.magnet.is_empty() {
                result.magnet.clone()
            } else {
                result.torrent.clone()
            };
            if url.is_empty() {
                continue;
            }

            match client.add_torrent(&url).await {
                Ok(_) => {
                    total_upgrades_grabbed += 1;
                    let tier = quality::detect_tier(&result.title, &result.resolution);
                    logger::info(
                        &state.db,
                        LogCategory::Grab,
                        &format!("Upgrade grabbed: {}", result.title),
                        &format!(
                            "series={}, target={}, group={}, tier={}",
                            title, label, result.group, tier.label()
                        ),
                    )
                    .await;

                    // Record for post-processing and quality tags.
                    let mut ep_nums: Vec<i32> = match &target {
                        auto_search::SearchTarget::Episode(n) => vec![*n],
                        auto_search::SearchTarget::Single => vec![1],
                    };
                    if result.is_batch {
                        let parsed = auto_search::parse_release_numbers(&result.title);
                        if !parsed.is_empty() {
                            ep_nums = parsed.into_iter().collect();
                            ep_nums.sort_unstable();
                        }
                    }
                    let _ = crate::models::grabbed_torrents::record_grab(
                        &state.db,
                        &result.info_hash,
                        &result.title,
                        row.id,
                        &ep_nums,
                    )
                    .await;
                    for ep_num in &ep_nums {
                        let _ = episode_tags::record_grab(
                            &state.db,
                            row.id,
                            *ep_num,
                            tier.label(),
                            &result.title,
                            &result.group,
                        )
                        .await;
                    }
                }
                Err(err) => {
                    logger::error(
                        &state.db,
                        LogCategory::QBit,
                        &format!("Upgrade grab failed: {} {}", title, label),
                        &err,
                    )
                    .await;
                }
            }

            // Rate-limit between searches to avoid hammering Nyaa.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        // Rate-limit between series.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    let detail = format!(
        "Checked {} series, {} episodes, grabbed {} upgrades",
        total_series_checked, total_episodes_checked, total_upgrades_grabbed
    );
    logger::info(
        &state.db,
        LogCategory::AutoSearch,
        "Upgrade search finished",
        &detail,
    )
    .await;

    Ok(UpgradeSummary {
        series_checked: total_series_checked,
        episodes_checked: total_episodes_checked,
        upgrades_grabbed: total_upgrades_grabbed,
        detail,
    })
}
