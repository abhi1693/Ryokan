use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};

use std::time::{Duration, Instant};
use tokio::sync::Notify;

use sqlx::SqlitePool;

use crate::models::config::Config;
use crate::models::log::LogCategory;
use crate::services::custom_formats::{self, CompiledCustomFormat};
use crate::services::source::{self, ClassificationResult, Resolution, Source};
use crate::services::{
    anilist::AnimeDetail,
    logger, media,
    nyaa::{self, SearchOptions, SearchResult},
    quality, seadex,
};

// ── Pre-compiled regexes for parse_release_numbers ─────────────────────────

mod aliases;
mod pack_detection;
mod release_parse;
mod scoring;
mod search_target;

use aliases::{SiblingRejectPrecompute, sibling_match_rejects};
pub use aliases::{
    collect_aliases, collect_extended_aliases, collect_sibling_aliases, dedupe_strings,
    matches_target, normalize_title, token_overlap_ratio, token_set,
};
pub use pack_detection::{
    TRANSITIVE_WALK_MAX_FETCHES, detect_sibling_entries_in_pack,
    expand_parent_with_transitive_relations, is_transitive_walk_source,
};
pub(crate) use release_parse::is_media_filename;
pub use release_parse::{
    has_selective_discriminator, infer_season_from_detail, parse_release_numbers,
    pick_wanted_file_indices,
};
use release_parse::{
    normalize_subtitle, season_mismatch, trailing_subtitle_of, within_episode_slack,
};
use scoring::{
    apply_cf_seadex_overlay, apply_cf_seadex_overlay_with_breakdown, rescore_for_auto_search,
    rescore_for_auto_search_with_breakdown,
};
pub use search_target::{
    SearchTarget, build_missing_targets, build_monitored_targets, build_upgrade_targets,
};

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct AutoSearchHit {
    pub target_label: String,
    pub release_title: String,
    pub release_group: String,
    pub quality_tier: String,
    pub url: String,
    pub score: i32,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct AutoSearchReport {
    pub grabbed: Vec<AutoSearchHit>,
    pub skipped: Vec<String>,
    pub quality_profile: String,
}

/// Return all scored candidates for an episode target without grabbing anything.
/// Used by the interactive search feature. More permissive than auto-search:
/// allows batch results and uses relaxed title matching so users see a broader
/// set of candidates to choose from.
pub async fn find_all_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    _allow_batch: bool,
    cfs: &[CompiledCustomFormat],
) -> Vec<SearchResult> {
    let aliases = collect_aliases(detail);
    let series_ctx = resolve_search_overrides(db, detail, config).await;
    let queries = append_custom_tokens(
        build_queries_from_aliases(&aliases, target, !series_ctx.restrict_user.is_empty()),
        &series_ctx.custom_tokens,
    );
    let preferred_groups = quality::parse_group_list(&config.preferred_groups);
    let preferred_res = preferred_resolution_search_value(config);
    let is_finished = detail.is_finished();
    let finished_mode = quality::FinishedSeriesMode::from_str(&config.finished_series_quality);
    let preferred_source_enum = Source::from_str(&config.preferred_source);
    let preferred_resolution_enum = Resolution::from_str(&config.preferred_resolution);
    // Scoring only looks at Source rank, so drop the BluRay sub-tier.
    let (cutoff_source_enum, _, _) = source::parse_cutoff_source(&config.cutoff_source);
    let cutoff_resolution_enum = Resolution::from_str(&config.cutoff_resolution);

    // Single SeaDex lookup per entry-point call, reused across every
    // candidate in the loop below. `seadex_gates` decides whether a
    // lookup is needed and whether the hardcoded boost is active —
    // it's suppressed automatically when the user has a
    // `SeaDexBestSpecification` CF to avoid double counting.
    let (seadex_needs_lookup, seadex_boost_enabled) = seadex_gates(config, cfs);
    let seadex_payload = fetch_seadex_payload(
        db,
        seadex_needs_lookup,
        detail.id,
        display_title(detail),
        &preferred_groups,
        &preferred_res,
        true,
    )
    .await;
    let seadex_hashes = seadex_payload.hashes;

    let expected_season = infer_season_from_detail(detail);
    let sibling_aliases = collect_sibling_aliases(detail, &aliases);
    let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &sibling_aliases);
    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    // Seed the candidate pool with SeaDex-curated releases so they're
    // guaranteed to show up in the interactive search UI even when
    // Nyaa's text search would miss them entirely (smol-style
    // megapacks titled by season rather than entry).
    for result in seadex_payload.candidates {
        let dedupe_key = if !result.info_hash.is_empty() {
            result.info_hash.clone()
        } else {
            result.title.to_lowercase()
        };
        if seen.insert(dedupe_key) {
            candidates.push(result);
        }
    }

    let ctx = InteractiveQueryCtx {
        aliases: &aliases,
        sibling_precompute: &sibling_precompute,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        expected_season,
        seadex_hashes: &seadex_hashes,
        restrict_user: &series_ctx.restrict_user,
        absolute_offset: series_ctx.absolute_offset,
        categories: &categories,
    };

    // Interactive search: allow batch results so user can see & pick them,
    // but filter by season and episode to avoid showing wrong-season results.
    run_queries_interactive(&queries, ctx, &mut seen, &mut candidates).await;

    // Try extended aliases if primary queries found nothing. Extended
    // aliases expand the own-side of the sibling-rejection comparison,
    // so rebuild the precompute with the full alias list — otherwise
    // an extended alias that legitimately overlaps with a sibling (e.g.
    // a synonym that happens to share tokens) would look like a sibling
    // win and get rejected.
    if candidates.is_empty() {
        let extended = collect_extended_aliases(detail);
        if !extended.is_empty() {
            let ext_queries = append_custom_tokens(
                build_queries_from_aliases(&extended, target, !series_ctx.restrict_user.is_empty()),
                &series_ctx.custom_tokens,
            );
            let all_aliases = [aliases.clone(), extended].concat();
            let ext_precompute = SiblingRejectPrecompute::build(&all_aliases, &sibling_aliases);
            let ext_ctx = InteractiveQueryCtx {
                aliases: &all_aliases,
                sibling_precompute: &ext_precompute,
                ..ctx
            };
            run_queries_interactive(&ext_queries, ext_ctx, &mut seen, &mut candidates).await;
        }
    }

    // #23 follow-up — When a Nyaa uploader restriction is active, every
    // Nyaa request is already scoped to `/user/<name>`, so a
    // preferred-group-prefixed query like "Erai-raws <title>" against the
    // SubsPlease user page can only return uploads SubsPlease happened to
    // name with "Erai-raws" in them — effectively never. Skip the whole
    // pass to avoid paying N × round-trip cost for zero coverage.
    if !preferred_groups.is_empty() && series_ctx.restrict_user.is_empty() {
        let group_queries = append_custom_tokens(
            build_group_queries(detail, target, &preferred_groups),
            &series_ctx.custom_tokens,
        );
        run_queries_interactive(&group_queries, ctx, &mut seen, &mut candidates).await;
    }

    // #30 Phase 2: franchise-root aliases + absolute episode number.
    // SubsPlease-style releases ("[SubsPlease] Jujutsu Kaisen - 56" for
    // JJK S3 E9) use the base franchise title and an absolute episode
    // number. Phase 1 drops them at the alias-match step because the
    // cour-specific aliases ("JJK: Shimetsu Kaiyuu - Zenpen",
    // "JUJUTSU KAISEN Season 3: The Culling Game Part 1") share only
    // 2 tokens with the release, below the 0.5 overlap threshold.
    //
    // This pass runs with a different ctx that treats franchise aliases
    // as the own-side, the computed absolute episode as the target, and
    // an empty sibling set (the base franchise name trivially substring-
    // matches every sibling in the graph, so re-using Phase 1's sibling
    // list would reject every absolute-numbered release).
    let franchise_precompute;
    let absolute_target;
    if series_ctx.absolute_offset > 0
        && !series_ctx.franchise_aliases.is_empty()
        && let SearchTarget::Episode(ep) = target
    {
        absolute_target = SearchTarget::Episode(ep.saturating_add(series_ctx.absolute_offset));
        franchise_precompute = SiblingRejectPrecompute::build(&series_ctx.franchise_aliases, &[]);
        let franchise_queries = append_custom_tokens(
            build_queries_from_aliases(
                &series_ctx.franchise_aliases,
                &absolute_target,
                !series_ctx.restrict_user.is_empty(),
            ),
            &series_ctx.custom_tokens,
        );
        let franchise_ctx = InteractiveQueryCtx {
            aliases: &series_ctx.franchise_aliases,
            sibling_precompute: &franchise_precompute,
            target: &absolute_target,
            // `target` already carries the absolute number, so no
            // secondary offset on top of that.
            absolute_offset: 0,
            ..ctx
        };
        run_queries_interactive(
            &franchise_queries,
            franchise_ctx,
            &mut seen,
            &mut candidates,
        )
        .await;
    }

    // Interactive search is user-driven — we want to *show* the
    // CF-filtered candidates even when they'd be dropped by an
    // auto-search path, so the minimum_score floor is suppressed here
    // (passed as `i32::MIN`). The CF score still contributes to ranking
    // so the user sees the same ordering the auto-picker would have used.
    let mut scored: Vec<SearchResult> = Vec::with_capacity(candidates.len());
    for mut c in candidates.drain(..) {
        let classification = source::classify_release(
            db,
            &c.title,
            Some(&c.resolution),
            Some(source::NyaaContext {
                info_hash: &c.info_hash,
                view_url: &c.link,
                is_batch: c.is_batch,
            }),
            Some(source::SeriesContext {
                status: &detail.status,
                season_year: detail.season_year,
                end_year: detail.end_year,
            }),
        )
        .await;
        // Interactive search uses the breakdown variants so each
        // candidate's `score_breakdown` stays in sync with its final
        // displayed score — the UI expander wants the full trail of
        // alias match / season penalty / CF contributions visible.
        let (base, mut auto_parts) = rescore_for_auto_search_with_breakdown(
            &c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
            series_ctx.absolute_offset,
            false, // batch_search_mode — episode target, single-unit penalty applies
        );
        // No CF floor on the interactive path — see comment above.
        if let Some((final_score, cf_parts)) = apply_cf_seadex_overlay_with_breakdown(
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            i32::MIN,
        ) {
            c.score = final_score;
            c.score_breakdown.append(&mut auto_parts);
            c.score_breakdown.extend(cf_parts);
            scored.push(c);
        }
    }

    scored.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    scored
}

pub async fn find_best_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    allow_batch: bool,
    batch_episode_match: bool,
    cfs: &[CompiledCustomFormat],
) -> Option<SearchResult> {
    collect_scored_for_target(
        db,
        detail,
        config,
        target,
        allow_batch,
        batch_episode_match,
        cfs,
    )
    .await
    .into_iter()
    .next()
}

/// Same multi-phase auto-search as `find_best_for_target`, but picks the
/// best *batch* release instead of the best overall. Two things had to
/// change relative to the pre-existing `best + filter(is_batch)` approach
/// that this function replaces:
///
/// 1. Filtering to `is_batch` happens *before* selection. The old code
///    picked the overall best scored candidate and then filtered, which
///    returned `None` whenever the top-scored result was a single-episode
///    weekly release — i.e. for almost every popular currently- or
///    recently-finished show.
/// 2. An extra batch-probe query phase runs alongside the standard query
///    sweep. Nyaa page 1 for a plain title query on a popular show is
///    dominated by weekly single-episode uploads; batches get pushed off
///    the first page entirely. The "X batch" / "X complete" / "X 01-"
///    probes funnel toward listings whose titles carry those tokens, so
///    batches surface even when the generic queries would miss them.
pub async fn find_best_batch_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    cfs: &[CompiledCustomFormat],
) -> Option<SearchResult> {
    collect_scored_batches_for_target(db, detail, config, target, cfs)
        .await
        .into_iter()
        .next()
}

/// Collection + scoring variant focused on batch releases.
///
/// Runs the same Phase 1/1.5/2/3 query sweep as the standard auto-search
/// but augments it with `quality::batch_probe_queries` to surface batches
/// that generic queries would miss on Nyaa page 1. Non-batch candidates
/// are dropped before scoring, so the returned `Vec` only contains batch
/// releases sorted by score descending.
pub async fn collect_scored_batches_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    cfs: &[CompiledCustomFormat],
) -> Vec<SearchResult> {
    let aliases = collect_aliases(detail);
    let series_ctx = resolve_search_overrides(db, detail, config).await;
    let preferred_groups = quality::parse_group_list(&config.preferred_groups);
    let preferred_res = preferred_resolution_search_value(config);
    let is_finished = detail.is_finished();
    let finished_mode = quality::FinishedSeriesMode::from_str(&config.finished_series_quality);
    let preferred_source_enum = Source::from_str(&config.preferred_source);
    let preferred_resolution_enum = Resolution::from_str(&config.preferred_resolution);
    let (cutoff_source_enum, _, _) = source::parse_cutoff_source(&config.cutoff_source);
    let cutoff_resolution_enum = Resolution::from_str(&config.cutoff_resolution);

    let (seadex_needs_lookup, seadex_boost_enabled) = seadex_gates(config, cfs);
    let seadex_payload = fetch_seadex_payload(
        db,
        seadex_needs_lookup,
        detail.id,
        display_title(detail),
        &preferred_groups,
        &preferred_res,
        true,
    )
    .await;
    let seadex_hashes = seadex_payload.hashes;

    let expected_season = infer_season_from_detail(detail);
    let sibling_aliases = collect_sibling_aliases(detail, &aliases);
    let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &sibling_aliases);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    // Seed with SeaDex-curated candidates fetched directly from their
    // view URLs. See `find_all_for_target` for the rationale — the
    // text-query sweep can't find batches whose titles don't carry
    // the target's alias tokens.
    for result in seadex_payload.candidates {
        let dedupe_key = if !result.info_hash.is_empty() {
            result.info_hash.clone()
        } else {
            result.title.to_lowercase()
        };
        if seen.insert(dedupe_key) {
            candidates.push(result);
        }
    }

    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);

    let ctx = AutoQueryCtx {
        aliases: &aliases,
        sibling_precompute: &sibling_precompute,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        allow_batch: true,
        expected_season,
        categories: &categories,
        batch_episode_match: false,
        seadex_hashes: &seadex_hashes,
        restrict_user: &series_ctx.restrict_user,
        absolute_offset: series_ctx.absolute_offset,
    };

    // Standard query sweep — picks up any batches that happen to surface
    // on Nyaa page 1 alongside the singles.
    let queries = append_custom_tokens(
        build_queries_from_aliases(&aliases, target, !series_ctx.restrict_user.is_empty()),
        &series_ctx.custom_tokens,
    );
    run_queries(&queries, ctx, &mut seen, &mut candidates).await;

    // Batch-targeted probes — the important addition for this function.
    // Explicit "batch" / "complete" keywords push the Nyaa search toward
    // listings that wouldn't appear on page 1 for a plain title query.
    let batch_queries = append_custom_tokens(
        quality::batch_probe_queries(&aliases),
        &series_ctx.custom_tokens,
    );
    run_queries(&batch_queries, ctx, &mut seen, &mut candidates).await;

    // Preferred-group queries, scoped to batches. Same fallback rule as
    // `collect_scored_for_target`: only fire if no preferred-group hit
    // has surfaced yet.
    let has_preferred_hit = !preferred_groups.is_empty()
        && candidates.iter().any(|c| {
            preferred_groups
                .iter()
                .any(|g| g.eq_ignore_ascii_case(&c.group))
        });
    // #23 follow-up — see the note in `find_all_for_target`. Preferred-
    // group queries are redundant when the `/user/<name>` scope is
    // already active.
    if !has_preferred_hit && !preferred_groups.is_empty() && series_ctx.restrict_user.is_empty() {
        let group_queries = append_custom_tokens(
            build_group_queries(detail, target, &preferred_groups),
            &series_ctx.custom_tokens,
        );
        run_queries(&group_queries, ctx, &mut seen, &mut candidates).await;
    }

    // Drop non-batches before the classify/rescore pass so we don't pay
    // the classification cost on candidates we're going to throw away.
    // SeaDex-curated candidates are exempt: the curator has already
    // blessed the release for this entry, and `detect_batch` misses
    // title forms like Roman-numeral season markers ("Mob Psycho 100
    // III") which are common in SeaDex picks. Without this exemption,
    // a curated full-season BD pack gets dropped here before it can be
    // scored.
    candidates.retain(|c| c.is_batch || is_seadex_match(&c.info_hash, &seadex_hashes));

    let mut scored: Vec<SearchResult> = Vec::with_capacity(candidates.len());
    for mut c in candidates.drain(..) {
        let classification = source::classify_release(
            db,
            &c.title,
            Some(&c.resolution),
            Some(source::NyaaContext {
                info_hash: &c.info_hash,
                view_url: &c.link,
                is_batch: c.is_batch,
            }),
            Some(source::SeriesContext {
                status: &detail.status,
                season_year: detail.season_year,
                end_year: detail.end_year,
            }),
        )
        .await;

        if is_finished
            && finished_mode == quality::FinishedSeriesMode::BdOnly
            && !source::passes_bd_only_filter(&classification)
        {
            continue;
        }

        // `collect_scored_batches_for_target` feeds both the user-facing
        // `interactive_search_batches` and the auto-grab
        // `find_best_batch_for_target`. Populating the breakdown here
        // costs a small Vec allocation per candidate on the auto path
        // too, which is cheap enough vs. the classify+network work that
        // already dominates the per-candidate cost.
        let (base, mut auto_parts) = rescore_for_auto_search_with_breakdown(
            &c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
            series_ctx.absolute_offset,
            true, // batch_search_mode — every candidate is a batch here
        );
        if let Some((final_score, cf_parts)) = apply_cf_seadex_overlay_with_breakdown(
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            config.custom_format_minimum_score,
        ) {
            c.score = final_score;
            c.score_breakdown.append(&mut auto_parts);
            c.score_breakdown.extend(cf_parts);
            scored.push(c);
        }
    }

    scored.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    scored
}

/// Internal: run the full auto-search query sweep (Phase 1 primary →
/// Phase 1.5 extended aliases → Phase 2 preferred-group queries →
/// Phase 3 BD probe), classify each candidate exactly once, filter via
/// the BdOnly rule, rescore, and return the sorted `Vec<SearchResult>`.
///
/// Factored out so `find_best_for_target` (picks the top result) and
/// `find_best_batch_for_target` (picks the top batch) can share the
/// expensive collection pass. Filtering to batches post-sort gives the
/// same answer as filtering pre-scoring because `rescore_for_auto_search`
/// applies its per-target batch bump uniformly inside each target kind.
async fn collect_scored_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    allow_batch: bool,
    batch_episode_match: bool,
    cfs: &[CompiledCustomFormat],
) -> Vec<SearchResult> {
    let aliases = collect_aliases(detail);
    let series_ctx = resolve_search_overrides(db, detail, config).await;
    let queries = append_custom_tokens(
        build_queries_from_aliases(&aliases, target, !series_ctx.restrict_user.is_empty()),
        &series_ctx.custom_tokens,
    );
    let preferred_groups = quality::parse_group_list(&config.preferred_groups);
    let preferred_res = preferred_resolution_search_value(config);
    let is_finished = detail.is_finished();
    let finished_mode = quality::FinishedSeriesMode::from_str(&config.finished_series_quality);
    let preferred_source_enum = Source::from_str(&config.preferred_source);
    let preferred_resolution_enum = Resolution::from_str(&config.preferred_resolution);
    // Scoring only looks at Source rank, so drop the BluRay sub-tier.
    let (cutoff_source_enum, _, _) = source::parse_cutoff_source(&config.cutoff_source);
    let cutoff_resolution_enum = Resolution::from_str(&config.cutoff_resolution);

    let (seadex_needs_lookup, seadex_boost_enabled) = seadex_gates(config, cfs);
    let seadex_payload = fetch_seadex_payload(
        db,
        seadex_needs_lookup,
        detail.id,
        display_title(detail),
        &preferred_groups,
        &preferred_res,
        true,
    )
    .await;
    let seadex_hashes = seadex_payload.hashes;

    let expected_season = infer_season_from_detail(detail);
    let sibling_aliases = collect_sibling_aliases(detail, &aliases);
    let sibling_precompute = SiblingRejectPrecompute::build(&aliases, &sibling_aliases);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    // Seed with SeaDex-curated candidates fetched directly from their
    // view URLs — this is how the smol Kizumonogatari pack (titled
    // `[smol] Monogatari (Season 9) ...`) gets into the pool for a
    // Kizumonogatari Part 2 target whose text queries would never
    // match the smol filename.
    //
    // Per-episode auto-search targets (`allow_batch=false`) mean "don't
    // add batches in this search" — the user has explicitly opted out
    // of batch grabs for episode search. Without this filter, every
    // episode search on a SeaDex-curated series with a megapack
    // top-hit would resurrect that batch into the candidate pool,
    // bypassing the setting. SeaDex curation does not override the
    // user's batch-allowed policy; it only overrides the heuristic
    // title-matching gate inside `run_queries`.
    for result in seadex_payload.candidates {
        if !allow_batch && result.is_batch {
            continue;
        }
        let dedupe_key = if !result.info_hash.is_empty() {
            result.info_hash.clone()
        } else {
            result.title.to_lowercase()
        };
        if seen.insert(dedupe_key) {
            candidates.push(result);
        }
    }

    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);

    let ctx = AutoQueryCtx {
        aliases: &aliases,
        sibling_precompute: &sibling_precompute,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        allow_batch,
        expected_season,
        categories: &categories,
        batch_episode_match,
        seadex_hashes: &seadex_hashes,
        restrict_user: &series_ctx.restrict_user,
        absolute_offset: series_ctx.absolute_offset,
    };

    // Phase 1: standard queries (primary aliases + episode variants).
    run_queries(&queries, ctx, &mut seen, &mut candidates).await;

    // Phase 1.5: if no candidates, try extended aliases (synonyms +
    // decomposed sub-phrases). Rebuild the sibling precompute with the
    // full alias list so own-vs-sibling overlap comparisons see the
    // extended aliases too.
    if candidates.is_empty() {
        let extended = collect_extended_aliases(detail);
        if !extended.is_empty() {
            let ext_queries = append_custom_tokens(
                build_queries_from_aliases(&extended, target, !series_ctx.restrict_user.is_empty()),
                &series_ctx.custom_tokens,
            );
            let all_aliases = [aliases.clone(), extended].concat();
            let ext_precompute = SiblingRejectPrecompute::build(&all_aliases, &sibling_aliases);
            let ext_ctx = AutoQueryCtx {
                aliases: &all_aliases,
                sibling_precompute: &ext_precompute,
                ..ctx
            };
            run_queries(&ext_queries, ext_ctx, &mut seen, &mut candidates).await;
        }
    }

    // Phase 2: if no candidate from a preferred group, try group-prefixed queries.
    let has_preferred_hit = !preferred_groups.is_empty()
        && candidates.iter().any(|c| {
            preferred_groups
                .iter()
                .any(|g| g.eq_ignore_ascii_case(&c.group))
        });

    // #23 follow-up — see the note in `find_all_for_target`. Preferred-
    // group queries are redundant when the `/user/<name>` scope is
    // already active.
    if !has_preferred_hit && !preferred_groups.is_empty() && series_ctx.restrict_user.is_empty() {
        let group_queries = append_custom_tokens(
            build_group_queries(detail, target, &preferred_groups),
            &series_ctx.custom_tokens,
        );
        run_queries(&group_queries, ctx, &mut seen, &mut candidates).await;
    }

    // Phase 3: for finished series with BD preference, probe for BD releases.
    // The "any BD candidate" check uses a filename-only heuristic so we can
    // decide before running the full classification pass.
    if is_finished && finished_mode != quality::FinishedSeriesMode::SameAsAiring {
        let has_bd_candidate = candidates
            .iter()
            .any(|c| source::looks_like_bluray_filename(&c.title));

        if !has_bd_candidate {
            let bd_queries = append_custom_tokens(
                quality::bd_probe_queries(&aliases),
                &series_ctx.custom_tokens,
            );
            run_queries(&bd_queries, ctx, &mut seen, &mut candidates).await;
        }
    }

    // #30 Phase 4: franchise-root aliases + absolute episode number.
    // Mirrors the interactive path — see the equivalent block in
    // `find_all_for_target` for the full rationale. SubsPlease-style
    // absolute-numbered releases for sequel cours ("Jujutsu Kaisen -
    // 56" for JJK S3 E9) need this pass to surface, otherwise the
    // cour-specific aliases reject them at the overlap threshold even
    // when Phase 1 queried for the absolute number.
    let franchise_precompute;
    let absolute_target;
    if series_ctx.absolute_offset > 0
        && !series_ctx.franchise_aliases.is_empty()
        && let SearchTarget::Episode(ep) = target
    {
        absolute_target = SearchTarget::Episode(ep.saturating_add(series_ctx.absolute_offset));
        franchise_precompute = SiblingRejectPrecompute::build(&series_ctx.franchise_aliases, &[]);
        let franchise_queries = append_custom_tokens(
            build_queries_from_aliases(
                &series_ctx.franchise_aliases,
                &absolute_target,
                !series_ctx.restrict_user.is_empty(),
            ),
            &series_ctx.custom_tokens,
        );
        let franchise_ctx = AutoQueryCtx {
            aliases: &series_ctx.franchise_aliases,
            sibling_precompute: &franchise_precompute,
            target: &absolute_target,
            absolute_offset: 0,
            ..ctx
        };
        run_queries(
            &franchise_queries,
            franchise_ctx,
            &mut seen,
            &mut candidates,
        )
        .await;
    }

    // Classify + filter + rescore in one pass. Each candidate is classified
    // exactly once, and both the BdOnly filter and the classification-aware
    // scoring reuse that single result.
    let mut scored: Vec<SearchResult> = Vec::with_capacity(candidates.len());
    for mut c in candidates.drain(..) {
        let classification = source::classify_release(
            db,
            &c.title,
            Some(&c.resolution),
            Some(source::NyaaContext {
                info_hash: &c.info_hash,
                view_url: &c.link,
                is_batch: c.is_batch,
            }),
            Some(source::SeriesContext {
                status: &detail.status,
                season_year: detail.season_year,
                end_year: detail.end_year,
            }),
        )
        .await;

        // BdOnly filter: drop non-BluRay releases for finished series when the
        // user has asked for BD only. Unknown sources get a pass.
        if is_finished
            && finished_mode == quality::FinishedSeriesMode::BdOnly
            && !source::passes_bd_only_filter(&classification)
        {
            continue;
        }

        let base = rescore_for_auto_search(
            &c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
            series_ctx.absolute_offset,
        );
        if let Some(final_score) = apply_cf_seadex_overlay(
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            config.custom_format_minimum_score,
        ) {
            c.score = final_score;
            scored.push(c);
        }
    }

    scored.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    scored
}

/// Shared context for `run_queries` — everything that stays constant
/// across the multi-phase query sweep inside `find_best_for_target`.
/// Bundling these into a struct (and away from the positional arg list)
/// closes a real foot-gun: the function used to take four back-to-back
/// `&[String]` slices (queries, aliases, preferred_groups, categories)
/// that the compiler would happily let you shuffle into the wrong
/// order. Named fields make the swap impossible. Derive `Copy` so the
/// Phase 1.5 alias override can reuse most fields via
/// `AutoQueryCtx { aliases: &all_aliases, ..ctx }`.
#[derive(Clone, Copy)]
struct AutoQueryCtx<'a> {
    aliases: &'a [String],
    /// Precomputed token sets for own + sibling aliases, used by
    /// [`sibling_match_rejects`] to reject a release that looks MORE
    /// like a sequel/prequel/side-story than the target. Built once
    /// at the top of the collect function so the ~50-candidates ×
    /// ~5-siblings normalize/tokenize loop runs once per sweep
    /// instead of once per candidate. See `collect_sibling_aliases`
    /// for the JJK S1→S3 motivating case.
    sibling_precompute: &'a SiblingRejectPrecompute,
    preferred_groups: &'a [String],
    preferred_resolution: &'a str,
    target: &'a SearchTarget,
    allow_batch: bool,
    expected_season: i32,
    categories: &'a [String],
    batch_episode_match: bool,
    /// Lowercase info hashes SeaDex has flagged as "best" for this
    /// target's AniList ID. A candidate whose hash is in this set
    /// bypasses the title/season/episode heuristic filters — SeaDex
    /// has already confirmed the release by AniList ID, so any
    /// title-based check is strictly inferior. Without this bypass,
    /// a smol/neoDESU-style release titled `Monogatari (Season 9)`
    /// would be rejected for a Kizumonogatari Part 2 target because
    /// `parse_release_season` would see "Season 9" and disagree
    /// with the Part-2 expected season.
    seadex_hashes: &'a HashSet<String>,
    /// #23 — Nyaa uploader name to restrict searches to. Goes straight
    /// into `SearchOptions.user`, which Nyaa translates to `?u=<name>`
    /// — server-side filter, so fewer/faster responses. Empty string
    /// means no restriction. Resolved from the per-series override or
    /// the global default at the entry point.
    restrict_user: &'a str,
    /// #30 — Cumulative episode count across the shortest TV-format
    /// PREQUEL chain up to this target. Allows an episode-filter match
    /// on either the relative number (target_ep, AL's own numbering)
    /// OR the absolute number (target_ep + absolute_offset, which is
    /// what SubsPlease-style TV releases use for sequel cours). Zero
    /// for first-season entries and for series whose relation cache
    /// hasn't populated yet, which collapses to the legacy
    /// strict-relative behavior.
    absolute_offset: i32,
}

/// Same idea, but for the interactive-search helper which has a
/// smaller shared context and no batch override.
#[derive(Clone, Copy)]
struct InteractiveQueryCtx<'a> {
    aliases: &'a [String],
    sibling_precompute: &'a SiblingRejectPrecompute,
    preferred_groups: &'a [String],
    preferred_resolution: &'a str,
    target: &'a SearchTarget,
    expected_season: i32,
    /// Nyaa category filter set — one of `1_2` (English-translated),
    /// `1_0` (Anime All, includes raws/foreign subs), or the MUSIC
    /// pair. Computed from `config.allow_non_english` at the entry
    /// point via `quality::nyaa_categories_for_format`. Previously the
    /// interactive path hardcoded `1_0`, which silently leaked raw
    /// Japanese releases and non-English-sub foreign releases into
    /// results even when the user had left "Allow non-English" off.
    categories: &'a [String],
    /// See the note on `AutoQueryCtx::seadex_hashes`. The interactive
    /// path's consequences for failing the bypass are more severe
    /// than the auto path: `run_queries_interactive` applies
    /// `season_mismatch` *unconditionally*, including for Single
    /// (movie) targets, where the auto path's `matches_target`
    /// skips it. That's why the smol Kizumonogatari II release
    /// vanished from interactive search even though auto search
    /// surfaced it.
    seadex_hashes: &'a HashSet<String>,
    /// #23 — see `AutoQueryCtx::restrict_user`.
    restrict_user: &'a str,
    /// #30 — see `AutoQueryCtx::absolute_offset`.
    absolute_offset: i32,
}

/// Run a set of queries against Nyaa page 1, collecting valid candidates.
async fn run_queries(
    queries: &[String],
    ctx: AutoQueryCtx<'_>,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<SearchResult>,
) {
    for category in ctx.categories {
        for query in queries {
            let opts = SearchOptions {
                query: query.clone(),
                category: category.clone(),
                filter: "0".to_string(),
                user: ctx.restrict_user.to_string(),
                preferred_groups: ctx.preferred_groups.to_vec(),
                preferred_resolution: ctx.preferred_resolution.to_string(),
                prefer_subs: true,
            };

            let resp = match nyaa::search(&opts, 1).await {
                Ok(v) => v,
                Err(_) => continue,
            };

            for result in resp.results {
                let dedupe_key = if !result.info_hash.is_empty() {
                    result.info_hash.clone()
                } else {
                    result.title.to_lowercase()
                };
                if !seen.insert(dedupe_key) {
                    continue;
                }
                // SeaDex trusts its AniList-ID-based curation over any
                // title heuristic. A hash match here means the release
                // is the community-curated best for this series, even
                // if its Nyaa title carries a season marker that would
                // otherwise fail `matches_target` (e.g. smol's
                // `Monogatari (Season 9)` release for a Kizumonogatari
                // Part 2 target).
                //
                // Batch filter runs unconditionally even for SeaDex
                // matches: an episode-search target with `allow_batch=
                // false` is an explicit "don't pull batches during
                // per-episode search" request from the user, and
                // silently letting SeaDex-curated batches through would
                // bypass that setting. SeaDex bypasses *heuristic* title
                // matching, not the user's batch-allowed policy.
                if !ctx.allow_batch && result.is_batch {
                    continue;
                }
                let is_seadex_best = is_seadex_match(&result.info_hash, ctx.seadex_hashes);
                if !is_seadex_best {
                    if !matches_target(
                        &result.title,
                        ctx.aliases,
                        ctx.sibling_precompute,
                        ctx.target,
                        ctx.expected_season,
                        ctx.batch_episode_match && result.is_batch,
                        ctx.absolute_offset,
                    ) {
                        continue;
                    }
                } else {
                    tracing::debug!(
                        "seadex: bypassing heuristic filters for SeaDex-best release title={:?} hash={}",
                        result.title,
                        result.info_hash
                    );
                }
                candidates.push(result);
            }
        }
    }
}

/// Run queries for interactive search with relaxed matching.
/// Uses relaxed alias matching (0.5 threshold) but still filters by season
/// and episode to avoid showing results from wrong seasons. Allows batch
/// results so users can see and pick them.
async fn run_queries_interactive(
    queries: &[String],
    ctx: InteractiveQueryCtx<'_>,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<SearchResult>,
) {
    for category in ctx.categories {
        for query in queries {
            let opts = SearchOptions {
                query: query.clone(),
                category: category.clone(),
                filter: "0".to_string(),
                user: ctx.restrict_user.to_string(),
                preferred_groups: ctx.preferred_groups.to_vec(),
                preferred_resolution: ctx.preferred_resolution.to_string(),
                prefer_subs: true,
            };

            let resp = match nyaa::search(&opts, 1).await {
                Ok(v) => v,
                Err(_) => continue,
            };

            for result in resp.results {
                let dedupe_key = if !result.info_hash.is_empty() {
                    result.info_hash.clone()
                } else {
                    result.title.to_lowercase()
                };
                if !seen.insert(dedupe_key) {
                    continue;
                }
                // SeaDex trusts its AniList-ID-based curation over any
                // title heuristic. If this hash is in the set, skip all
                // alias / season / episode checks below — the unconditional
                // `season_mismatch` in particular drops releases like
                // smol's `Monogatari (Season 9)` for a Kizumonogatari Part
                // 2 target, even though SeaDex has already confirmed the
                // AniList ID match.
                if is_seadex_match(&result.info_hash, ctx.seadex_hashes) {
                    tracing::debug!(
                        "seadex: bypassing heuristic filters for SeaDex-best release title={:?} hash={}",
                        result.title,
                        result.info_hash
                    );
                    candidates.push(result);
                    continue;
                }
                // Relaxed alias matching: lower threshold than auto search
                let normalized_title = normalize_title(&result.title);
                let title_tokens = token_set(&normalized_title);
                let alias_match = ctx.aliases.iter().any(|alias| {
                    let normalized_alias = normalize_title(alias);
                    normalized_title.contains(&normalized_alias)
                        || token_overlap_ratio(&title_tokens, &token_set(&normalized_alias)) >= 0.5
                });
                if !alias_match {
                    continue;
                }
                // Sibling rejection: same sequel/prequel guard as the auto
                // path — a release that matches a sibling more tightly than
                // us is almost certainly for the sibling.
                if sibling_match_rejects(&normalized_title, &title_tokens, ctx.sibling_precompute) {
                    continue;
                }
                // Season check: reject results clearly from a different season
                if season_mismatch(&result.title, ctx.expected_season) {
                    continue;
                }
                // Episode check for single-episode targets (allow batches through).
                // #30 — A release passes if its parsed number matches either the
                // relative target (AL's per-cour numbering) OR the absolute
                // number `target + absolute_offset` (what SubsPlease-style TV
                // releases use for sequel cours, e.g. JJK S3 E9 shipped as
                // "Jujutsu Kaisen - 56" with offset 47). When offset is 0 this
                // collapses to the legacy strict-relative behavior.
                if let SearchTarget::Episode(target_ep) = ctx.target
                    && !result.is_batch
                {
                    let parsed = parse_release_numbers(&result.title);
                    if !parsed.is_empty()
                        && !episode_match(&parsed, *target_ep, ctx.absolute_offset)
                    {
                        continue;
                    }
                }
                candidates.push(result);
            }
        }
    }
}

/// #30 — Episode-filter acceptance check. A release's parsed episode
/// numbers match the target when they carry either the relative target
/// number (AL's own per-cour numbering) or the absolute number derived
/// by adding the cumulative prior-cour episode count. `offset == 0`
/// reduces to the strict-relative path used for first-season entries
/// and for series whose relation cache hasn't populated yet.
pub(super) fn episode_match(parsed: &HashSet<i32>, target_ep: i32, absolute_offset: i32) -> bool {
    if parsed.contains(&target_ep) {
        return true;
    }
    if absolute_offset > 0 {
        let absolute = target_ep.saturating_add(absolute_offset);
        if parsed.contains(&absolute) {
            return true;
        }
    }
    false
}

/// Build group-prefixed queries for the fallback search.
/// e.g. "SubsPlease Jujutsu Kaisen - 01", "SubsPlease Jujutsu Kaisen 01"
fn build_group_queries(
    detail: &AnimeDetail,
    target: &SearchTarget,
    preferred_groups: &[String],
) -> Vec<String> {
    let aliases = collect_aliases(detail);
    let mut queries = Vec::new();

    for group in preferred_groups {
        for alias in &aliases {
            match target {
                SearchTarget::Single => {
                    queries.push(format!("{} {}", group, alias));
                }
                SearchTarget::Episode(ep) => {
                    queries.push(format!("{} {} - {:02}", group, alias, ep));
                    queries.push(format!("{} {} {:02}", group, alias, ep));
                }
            }
        }
    }

    dedupe_strings(queries)
}

/// Resolve the best-available classification for an on-disk episode. Public
/// so RSS upgrade detection can use the same hydration order.
pub fn resolve_existing_classification(
    file: &media::EpisodeFile,
    tag: Option<&crate::models::episode_tags::EpisodeQualityTag>,
) -> ClassificationResult {
    if let Some(tag) = tag {
        if !tag.source.is_empty() || !tag.resolution.is_empty() {
            return source::classification_from_stored_full(
                &tag.source,
                &tag.resolution,
                tag.is_remux,
                tag.is_bdmv,
                source::WebKind::from_str(&tag.web_kind),
                tag.classification_confidence,
                tag.needs_review,
            );
        }
        if !tag.release_title.is_empty() {
            return source::classify_release_sync(&tag.release_title, None);
        }
    }
    // No usable tag — fall back to the on-disk filename + parsed quality.
    source::classify_release_sync(&file.filename, Some(&file.quality))
}

pub fn target_label(target: &SearchTarget) -> String {
    match target {
        SearchTarget::Single => "Single".to_string(),
        SearchTarget::Episode(ep) => format!("Episode {}", ep),
    }
}

/// Per-series search context resolved from the `series` row (with
/// fallbacks to global `config` defaults for the user-controlled
/// overrides). One DB hit per entry-point call for the series row
/// plus one extra when `absolute_offset > 0` to walk the franchise
/// root titles.
struct SeriesSearchCtx {
    /// #23 — Extra tokens appended verbatim to every Nyaa query after
    /// the title aliases. Empty means no extra tokens.
    custom_tokens: String,
    /// #23 — Nyaa uploader name (`?u=<name>`) server-side filter.
    /// Empty means no restriction.
    restrict_user: String,
    /// #30 — Cumulative TV-cour episode count for the entry's PREQUEL
    /// chain. Zero for first-season entries and for series whose
    /// relation cache hasn't populated yet. Used by the episode filter
    /// to accept absolute-numbered Nyaa releases against a
    /// relative-numbered AL target.
    absolute_offset: i32,
    /// #30 — Titles of every TV-format ancestor on the PREQUEL chain.
    /// Used to build queries like `Jujutsu Kaisen 56` that a Nyaa text
    /// search will actually match against a SubsPlease-shaped release
    /// title. The cour-specific AL titles (e.g. "JUJUTSU KAISEN Season
    /// 3: The Culling Game Part 1", "Jujutsu Kaisen: Shimetsu Kaiyuu
    /// Zenpen") don't appear in SubsPlease release names, so without
    /// these franchise-root titles the absolute-numbered release is
    /// never in the candidate pool — loosening the filter alone is
    /// not enough. Empty for first-season entries.
    franchise_aliases: Vec<String>,
}

/// Resolve per-series search overrides + the cumulative-prior-episodes
/// offset, falling back to global defaults from `config`. Per-series
/// user overrides (`#23`) win when non-empty; the `#30` offset and
/// franchise aliases have no global default (both are derived from
/// the per-series relation cache).
async fn resolve_search_overrides(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
) -> SeriesSearchCtx {
    let row = crate::models::series::get_by_anilist_id(db, detail.id)
        .await
        .ok()
        .flatten();
    match row {
        Some(s) => resolve_search_overrides_from_row_async(db, &s, config).await,
        None => SeriesSearchCtx {
            custom_tokens: config.default_custom_query_tokens.clone(),
            restrict_user: config.default_restrict_to_uploader.clone(),
            // No series row means the entry isn't in the library yet;
            // no relation cache to pull an offset from, so the filter
            // stays strict-relative. This only affects provisional
            // Sonarr-shim searches for unadded series.
            absolute_offset: 0,
            franchise_aliases: Vec::new(),
        },
    }
}

/// Async entry-point variant — hits the DB for franchise aliases when
/// the series has a non-zero offset. The sync test variant below is
/// kept for unit tests that don't need the alias lookup.
async fn resolve_search_overrides_from_row_async(
    db: &SqlitePool,
    series: &crate::models::series::Series,
    config: &Config,
) -> SeriesSearchCtx {
    let mut ctx = resolve_search_overrides_from_row(series, config);
    if ctx.absolute_offset > 0 && series.anilist_id != 0 {
        ctx.franchise_aliases =
            crate::models::local_metadata::resolve_franchise_aliases(db, series.anilist_id).await;
    }
    ctx
}

fn resolve_search_overrides_from_row(
    series: &crate::models::series::Series,
    config: &Config,
) -> SeriesSearchCtx {
    let custom_tokens = if series.custom_query_tokens.is_empty() {
        config.default_custom_query_tokens.clone()
    } else {
        series.custom_query_tokens.clone()
    };
    let restrict_user = if series.restrict_to_uploader.is_empty() {
        config.default_restrict_to_uploader.clone()
    } else {
        series.restrict_to_uploader.clone()
    };
    SeriesSearchCtx {
        custom_tokens,
        restrict_user,
        absolute_offset: series.cumulative_prior_episodes.max(0),
        // Left empty in the sync variant — callers that need them use
        // the async variant. Tests pin the sync variant's behavior on
        // the other fields only.
        franchise_aliases: Vec::new(),
    }
}

/// #23 — Append user-supplied custom query tokens to every query in
/// the list. Empty tokens is a no-op so the common path stays
/// allocation-free. Tokens are appended verbatim — users can pass any
/// Nyaa query syntax (quoted phrases, minus-prefix exclusions, etc.)
/// that `build_queries_from_aliases` didn't generate.
fn append_custom_tokens(queries: Vec<String>, tokens: &str) -> Vec<String> {
    let trimmed = tokens.trim();
    if trimmed.is_empty() {
        return queries;
    }
    queries
        .into_iter()
        .map(|q| format!("{} {}", q, trimmed))
        .collect()
}

/// Build the Nyaa text-query variants for each alias. The full sweep
/// emits four variants per alias for Episode targets (`title 9`,
/// `title - 09`, `title 09`, `"title" 09`) to cover punctuation and
/// padding conventions across uploaders, plus two variants for Single
/// targets (bare + phrase-match).
///
/// #23 follow-up — When a Nyaa uploader restriction (`?u=<name>`) is
/// active those variants collapse to the same token set against a
/// single uploader's catalog: Nyaa's tokenizer ignores punctuation,
/// and the phrase-match variant narrows a result set that's already
/// narrowed by the server-side user filter. Running all four in
/// sequence burned 15–25s per sweep for no additional coverage.
/// `collapsed = true` emits a single canonical variant per alias —
/// the zero-padded episode form (`title 09`) for Episode targets, the
/// bare alias for Single targets — cutting the per-alias query count
/// 4→1 (Episode) and 2→1 (Single).
fn build_queries_from_aliases(
    aliases: &[String],
    target: &SearchTarget,
    collapsed: bool,
) -> Vec<String> {
    let mut queries = Vec::new();

    for alias in aliases {
        match target {
            SearchTarget::Single => {
                queries.push(alias.clone());
                if !collapsed {
                    queries.push(format!("\"{}\"", alias));
                }
            }
            SearchTarget::Episode(ep) => {
                if collapsed {
                    queries.push(format!("{} {:02}", alias, ep));
                } else {
                    queries.push(format!("{} {}", alias, ep));
                    queries.push(format!("{} - {:02}", alias, ep));
                    queries.push(format!("{} {:02}", alias, ep));
                    queries.push(format!("\"{}\" {:02}", alias, ep));
                }
            }
        }
    }

    dedupe_strings(queries)
}

/// Look up the SeaDex entry for `anilist_id` and return the set of
/// usable "best" info hashes. Disabled (or lookup-failed) returns an
/// empty set, which causes the scoring-time SeaDex bonus and any
/// `SeaDexBest` Custom Format spec to harmlessly contribute zero.
///
/// Emits both `tracing::debug!` lines (for `RUST_LOG=ryokan=debug`
/// console readers) and `LogCategory::AutoSearch` rows (for the
/// in-app Log Viewer) for every call — skip, hit, miss, and error.
/// The previous version silently swallowed errors into
/// `HashSet::new()`, which made a dead releases.moe indistinguishable
/// from "SeaDex not configured" or "this title isn't on SeaDex."
/// Everything the auto-search pipeline needs from one SeaDex lookup:
/// the set of "best" info hashes (for the filter bypass and score
/// overlay) and fully-populated `SearchResult` candidates built
/// directly from each curated torrent's Nyaa view page.
///
/// The pre-fetched candidates are the key to surfacing SeaDex releases
/// whose Nyaa titles don't overlap with the target's AniList aliases
/// (smol's `Monogatari (Season 9)` megapack for Kizumonogatari Part 2
/// is the canonical example). The text-query sweep can't find them;
/// we go direct to `/view/<id>` and inject the result ourselves.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct SeaDexPayload {
    hashes: HashSet<String>,
    /// Synthetic candidates fetched directly from each SeaDex-curated
    /// torrent's Nyaa view URL. Empty when the lookup is skipped or
    /// fails, or when every fetch fails. Merged into the candidate
    /// pool by the caller before the text-query sweep runs.
    candidates: Vec<SearchResult>,
}

/// 24-hour in-memory cache for SeaDex lookups, keyed by AniList ID.
///
/// A single auto-search sweep across a multi-target batch (`find_all_for_target`,
/// `collect_scored_batches_for_target`, `collect_scored_for_target`)
/// can round-trip releases.moe several times per target, and each hit
/// also fetches every SeaDex-best torrent's Nyaa view page. For a
/// JoJo S1–S5 sweep that's up to ~5 × (1 + N) HTTP requests — enough
/// to throttle both releases.moe and Nyaa on a cold start.
///
/// SeaDex is a curated dataset that updates on the order of days, not
/// minutes — once the community picks a "best" release for a title it
/// rarely churns — so a 24h TTL amortizes the cost down to ~1 lookup
/// per target per day while still catching the occasional revision.
/// Anything shorter burns network round-trips for no observable
/// correctness benefit. Config changes (preferred groups, resolution)
/// affect how candidates get *scored* downstream, not what SeaDex
/// returns, so keying by anilist_id alone is correct.
///
/// The cache lives for the lifetime of the process, so a restart is
/// the operator's escape hatch if they ever need to force-refresh.
const SEADEX_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
// Errors are cached for a much shorter window. The point is to absorb a
// transient releases.moe outage (or a brief 5xx burst) without every
// concurrent search hammering the upstream — but not so long that a
// recovered service stays masked across the next RSS sweep.
const SEADEX_ERROR_TTL: Duration = Duration::from_secs(5 * 60);
// Cap the cache so a long-running process can't accumulate every
// AniList ID it ever touched. Mirrors anilist::DETAIL_CACHE_MAX_ENTRIES.
const SEADEX_CACHE_MAX_ENTRIES: usize = 500;

/// Cache value carries an `expires_at` (rather than `fetched_at`) so the
/// success and error TTLs can coexist without `cache_get` having to know
/// which kind it is reading.
static SEADEX_CACHE: LazyLock<StdMutex<HashMap<i64, (Instant, SeaDexPayload)>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// In-flight registry — one `Notify` per anilist_id currently being
/// fetched. Concurrent callers find the existing entry, await the
/// notify, then re-check the cache. Without this, the cold-cache
/// window is a thundering-herd target: an RSS sweep, a manual button,
/// and an anibridge request can all fire on the same series in the
/// same second and each one hits releases.moe.
static SEADEX_INFLIGHT: LazyLock<StdMutex<HashMap<i64, Arc<Notify>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn seadex_cache_get(anilist_id: i64) -> Option<SeaDexPayload> {
    let cache = SEADEX_CACHE.lock().ok()?;
    let (expires_at, payload) = cache.get(&anilist_id)?;
    if Instant::now() < *expires_at {
        Some(payload.clone())
    } else {
        None
    }
}

fn seadex_cache_put_with_ttl(anilist_id: i64, payload: SeaDexPayload, ttl: Duration) {
    if let Ok(mut cache) = SEADEX_CACHE.lock() {
        let expires_at = Instant::now() + ttl;
        cache.insert(anilist_id, (expires_at, payload));
        if cache.len() > SEADEX_CACHE_MAX_ENTRIES {
            // Drop expired first; if still over cap, drop the entry
            // that expires soonest (effectively LRU under uniform TTL).
            let now = Instant::now();
            let expired: Vec<i64> = cache
                .iter()
                .filter(|(_, (expires_at, _))| *expires_at <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in &expired {
                cache.remove(k);
            }
            // Exclude the entry we just inserted from the soonest-expires
            // candidate set. Without this, an error entry (5-min TTL)
            // inserted into a cache full of fresh success entries (24h
            // TTL) immediately self-evicts because it's the row with the
            // earliest `expires_at` — defeating the negative-cache
            // coalescing the short TTL was added to provide.
            if cache.len() > SEADEX_CACHE_MAX_ENTRIES
                && let Some((&oldest, _)) = cache
                    .iter()
                    .filter(|(k, _)| **k != anilist_id)
                    .min_by_key(|(_, (expires_at, _))| *expires_at)
            {
                cache.remove(&oldest);
            }
        }
    }
}

fn seadex_cache_put(anilist_id: i64, payload: SeaDexPayload) {
    seadex_cache_put_with_ttl(anilist_id, payload, SEADEX_CACHE_TTL);
}

fn seadex_cache_put_error(anilist_id: i64) {
    seadex_cache_put_with_ttl(anilist_id, SeaDexPayload::default(), SEADEX_ERROR_TTL);
}

/// Persist a successful (or "no entry") lookup to SQLite so it survives
/// process restart. Called from the leader path on the success / no-entry
/// branches; the error branch deliberately doesn't persist (5-min TTL is
/// too short to be worth the I/O, and a restart should re-probe upstream
/// health rather than inherit a "this is broken" verdict).
async fn seadex_persist_to_db(db: &SqlitePool, anilist_id: i64, payload: &SeaDexPayload) {
    let json = match serde_json::to_string(payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("seadex: failed to serialize payload for persistence: {e}");
            return;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let res = sqlx::query(
        "INSERT INTO seadex_lookup_cache (anilist_id, payload_json, cached_at) \
         VALUES (?, ?, ?) \
         ON CONFLICT(anilist_id) DO UPDATE SET payload_json = excluded.payload_json, cached_at = excluded.cached_at",
    )
    .bind(anilist_id)
    .bind(&json)
    .bind(now)
    .execute(db)
    .await;
    if let Err(e) = res {
        tracing::warn!("seadex: failed to persist cache row for anilist_id={anilist_id}: {e}");
    }
}

/// Warm the in-memory SeaDex cache from SQLite at startup. Drops rows
/// older than `SEADEX_CACHE_TTL` opportunistically (cheap to run during
/// boot; avoids unbounded growth of the persisted table over time).
/// Called once from `main()` after migrations.
pub async fn seadex_warm_cache_from_db(db: &SqlitePool) {
    let ttl_secs = SEADEX_CACHE_TTL.as_secs() as i64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Drop expired persisted rows first so the SELECT below doesn't have
    // to filter them in Rust and the table stays bounded.
    if let Err(e) = sqlx::query("DELETE FROM seadex_lookup_cache WHERE cached_at + ? < ?")
        .bind(ttl_secs)
        .bind(now)
        .execute(db)
        .await
    {
        tracing::warn!("seadex: failed to evict expired persisted rows: {e}");
    }

    let rows = match sqlx::query_as::<_, (i64, String, i64)>(
        "SELECT anilist_id, payload_json, cached_at FROM seadex_lookup_cache",
    )
    .fetch_all(db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("seadex: failed to read persisted cache for warming: {e}");
            return;
        }
    };

    let mut warmed = 0usize;
    for (anilist_id, json, cached_at) in rows {
        let remaining_secs = (cached_at + ttl_secs).saturating_sub(now);
        if remaining_secs <= 0 {
            continue;
        }
        let payload: SeaDexPayload = match serde_json::from_str(&json) {
            Ok(p) => p,
            Err(e) => {
                // Schema drift in SearchResult or SeaDexPayload would
                // land here. Skip the row rather than aborting startup;
                // it'll be re-fetched on next lookup.
                tracing::warn!(
                    "seadex: skipping unparseable persisted row anilist_id={anilist_id}: {e}"
                );
                continue;
            }
        };
        seadex_cache_put_with_ttl(
            anilist_id,
            payload,
            Duration::from_secs(remaining_secs as u64),
        );
        warmed += 1;
    }
    tracing::info!("seadex: warmed {warmed} persisted cache entries from SQLite");
}

/// Pre-fetch SeaDex hits for many AniList ids in one OR-batched
/// PocketBase request and cache the *negative* responses (ids that
/// SeaDex doesn't know about) so the per-series loop downstream skips
/// the SeaDex round-trip for those ids entirely.
///
/// Designed for the upgrade-search sweep: most series in a typical
/// library have no SeaDex entry, so a single batched call (50 ids per
/// chunk) replaces N sequential round-trips for those ids. Hits are
/// deliberately NOT cached here — the per-series fetch path also pulls
/// Nyaa view-page candidates for each usable torrent in the entry,
/// which doesn't fit the "single batch query" shape; letting hits
/// flow through the lazy path keeps that work amortized across the
/// loop iterations rather than concentrated in a startup burst.
///
/// Already-cached ids (positive or negative) are skipped, so calling
/// this repeatedly within a TTL window is cheap. Failures are logged
/// and swallowed: the worst case is the per-series loop pays the
/// previously-existing per-id cost.
pub async fn prewarm_seadex_negative(db: &SqlitePool, anilist_ids: &[i64]) {
    let to_query: Vec<i64> = anilist_ids
        .iter()
        .copied()
        .filter(|id| {
            *id > 0
                && seadex_cache_get(*id).is_none()
                // Don't prewarm an id that's already being fetched by another
                // concurrent path (RSS sweep, manual button, anibridge request).
                // The leader's `seadex_cache_put` will populate the cache for us;
                // doubling the request would defeat the in-flight coalescing.
                && !seadex_inflight_contains(*id)
        })
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();
    if to_query.is_empty() {
        return;
    }
    tracing::debug!(
        "seadex: prewarming negative cache for {} anilist_id(s)",
        to_query.len()
    );
    let results = match seadex::lookup_batch(&to_query).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("seadex: batch prewarm failed: {e}");
            return;
        }
    };
    let mut cached = 0usize;
    for (anilist_id, entry) in results {
        if entry.is_none() {
            let payload = SeaDexPayload::default();
            seadex_cache_put(anilist_id, payload.clone());
            seadex_persist_to_db(db, anilist_id, &payload).await;
            cached += 1;
        }
    }
    tracing::info!(
        "seadex: prewarm cached {cached} negative entries from {} batched lookup(s)",
        to_query.len().div_ceil(seadex::SEADEX_BATCH_SIZE)
    );
}

/// Cheap non-blocking check for "is this anilist_id currently being
/// fetched by some other coalesced path?" Returns false on lock
/// poisoning so prewarm errs on the side of doing the work.
fn seadex_inflight_contains(anilist_id: i64) -> bool {
    SEADEX_INFLIGHT
        .lock()
        .map(|m| m.contains_key(&anilist_id))
        .unwrap_or(false)
}

/// Drop guard — removes the in-flight registry entry and wakes any
/// waiters even if the leader's fetch panics or returns early. Without
/// this, a stuck entry would block every future lookup for that
/// anilist_id until process restart.
struct SeaDexInFlightGuard {
    anilist_id: i64,
    notify: Arc<Notify>,
}
impl Drop for SeaDexInFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = SEADEX_INFLIGHT.lock() {
            map.remove(&self.anilist_id);
        }
        self.notify.notify_waiters();
    }
}

async fn fetch_seadex_payload(
    db: &SqlitePool,
    seadex_enabled: bool,
    anilist_id: i64,
    series_title: &str,
    preferred_groups: &[String],
    preferred_resolution: &str,
    prefer_subs: bool,
) -> SeaDexPayload {
    if !seadex_enabled {
        tracing::debug!(
            "seadex: skipping lookup — gate off (seadex_enabled=false and no SeaDex CF installed)"
        );
        // Intentionally no DB log row for the "gate off" path — this
        // would spam the Log Viewer with one line per search for every
        // user who hasn't turned SeaDex on.
        return SeaDexPayload::default();
    }
    if anilist_id <= 0 {
        tracing::debug!(
            "seadex: skipping lookup — no AniList ID on target (anilist_id={anilist_id})"
        );
        logger::debug(
            db,
            LogCategory::AutoSearch,
            &format!("SeaDex lookup skipped for {series_title}"),
            &format!("no AniList ID (anilist_id={anilist_id})"),
        )
        .await;
        return SeaDexPayload::default();
    }
    if let Some(cached) = seadex_cache_get(anilist_id) {
        tracing::debug!(
            "seadex: cache hit for anilist_id={anilist_id} ({} hash(es), {} candidate(s))",
            cached.hashes.len(),
            cached.candidates.len()
        );
        return cached;
    }

    // Leadership election. If another task is already fetching this
    // anilist_id, wait for it to finish and then re-read the cache.
    // Loop because a leader could finish without populating the cache
    // (defensive against future bail-outs); in that case we re-attempt
    // leadership ourselves rather than spinning on a notify that's
    // already been sent.
    //
    // The `Role` enum exists to keep the `StdMutex` guard out of the
    // async scope — `MutexGuard` is `!Send`, so we have to drop it
    // before any `.await` (the compiler can't see that `drop(inflight)`
    // happens before the await on its own).
    enum Role {
        Lead(SeaDexInFlightGuard),
        Wait(Arc<Notify>),
    }
    let _guard: Option<SeaDexInFlightGuard> = loop {
        let role = match SEADEX_INFLIGHT.lock() {
            Err(_) => {
                // Poisoned — skip coalescing entirely and just fetch.
                // Redundant network work beats wedging the path.
                break None;
            }
            Ok(mut inflight) => {
                if let Some(existing) = inflight.get(&anilist_id) {
                    Role::Wait(existing.clone())
                } else {
                    let notify = Arc::new(Notify::new());
                    inflight.insert(anilist_id, notify.clone());
                    Role::Lead(SeaDexInFlightGuard { anilist_id, notify })
                }
            }
        };
        // MutexGuard dropped at end of `match` expression above.
        match role {
            Role::Lead(g) => break Some(g),
            Role::Wait(notify) => {
                // Subscribe BEFORE re-checking the cache. `Notify::notify_waiters`
                // doesn't leave a permit for future `.notified()` calls — if the
                // leader fires the notify between our unlock above and our
                // first poll, we'd hang forever waiting for a notification
                // that already happened. The recipe from tokio's docs is
                // pin → enable → re-check → await: enabling registers our
                // waiter atomically against the next `notify_waiters`, so
                // any notification fired after `enable()` (including ones
                // that race with our cache re-check) wakes us correctly.
                let waiter = notify.notified();
                tokio::pin!(waiter);
                waiter.as_mut().enable();
                if let Some(cached) = seadex_cache_get(anilist_id) {
                    tracing::debug!("seadex: coalesced wait hit for anilist_id={anilist_id}");
                    return cached;
                }
                waiter.await;
                if let Some(cached) = seadex_cache_get(anilist_id) {
                    tracing::debug!("seadex: coalesced wait hit for anilist_id={anilist_id}");
                    return cached;
                }
                // Leader didn't populate; loop and try to lead ourselves.
                continue;
            }
        }
    };

    tracing::debug!("seadex: fetching releases.moe entry for anilist_id={anilist_id}");
    match seadex::lookup(anilist_id).await {
        Ok(Some(entry)) => {
            let hashes = seadex::best_hashes(&entry);
            tracing::debug!(
                "seadex: releases.moe returned {} usable hash(es) for anilist_id={}",
                hashes.len(),
                anilist_id
            );
            logger::debug(
                db,
                LogCategory::AutoSearch,
                &format!(
                    "SeaDex lookup: {} usable hash(es) for {series_title}",
                    hashes.len()
                ),
                &format!("anilist_id={anilist_id}"),
            )
            .await;

            // Fetch each usable torrent's view page in parallel via
            // JoinSet — a typical SeaDex entry has 1–4 usable torrents
            // and the previous serial loop turned that into 1–4 ×
            // ~500ms of wall time on cache miss. Concurrency is
            // self-bounded by `usable.len()`, so no semaphore needed.
            let opts_for_score = nyaa::SearchOptions {
                query: series_title.to_string(),
                category: "1_0".to_string(),
                filter: "0".to_string(),
                user: String::new(),
                preferred_groups: preferred_groups.to_vec(),
                preferred_resolution: preferred_resolution.to_string(),
                prefer_subs,
            };
            let mut join_set: tokio::task::JoinSet<(String, Result<SearchResult, String>)> =
                tokio::task::JoinSet::new();
            for torrent in entry.torrents.iter() {
                if !seadex::is_usable(torrent, &entry.notes) {
                    continue;
                }
                let view_url = seadex::to_nyaa_view_url(torrent).to_string();
                let opts = opts_for_score.clone();
                join_set.spawn(async move {
                    let result = nyaa::fetch_view_result(&view_url, &opts).await;
                    (view_url, result)
                });
            }
            let mut candidates = Vec::new();
            while let Some(joined) = join_set.join_next().await {
                match joined {
                    Ok((view_url, Ok(result))) => {
                        tracing::debug!(
                            "seadex: injected curated candidate from view url={} title={:?} hash={}",
                            view_url,
                            result.title,
                            result.info_hash
                        );
                        candidates.push(result);
                    }
                    Ok((view_url, Err(e))) => {
                        tracing::warn!("seadex: failed to fetch view page for {}: {}", view_url, e);
                        logger::warn(
                            db,
                            LogCategory::AutoSearch,
                            &format!("SeaDex view-page fetch failed for {series_title}"),
                            &format!("url={view_url}, error={e}"),
                        )
                        .await;
                    }
                    Err(join_err) => {
                        tracing::warn!("seadex: view-page task failed to join: {join_err}");
                    }
                }
            }
            let payload = SeaDexPayload { hashes, candidates };
            seadex_cache_put(anilist_id, payload.clone());
            seadex_persist_to_db(db, anilist_id, &payload).await;
            payload
        }
        Ok(None) => {
            tracing::debug!("seadex: releases.moe has no entry for anilist_id={anilist_id}");
            logger::debug(
                db,
                LogCategory::AutoSearch,
                &format!("SeaDex has no entry for {series_title}"),
                &format!("anilist_id={anilist_id}"),
            )
            .await;
            // Cache the "no entry" result so we don't re-hit releases.moe
            // for the same anilist_id within the TTL window.
            let payload = SeaDexPayload::default();
            seadex_cache_put(anilist_id, payload.clone());
            seadex_persist_to_db(db, anilist_id, &payload).await;
            payload
        }
        Err(e) => {
            tracing::warn!("seadex: releases.moe lookup failed for anilist_id={anilist_id}: {e}");
            logger::warn(
                db,
                LogCategory::AutoSearch,
                &format!("SeaDex lookup failed for {series_title}"),
                &format!("anilist_id={anilist_id}, error={e}"),
            )
            .await;
            // Negative-cache the failure (short TTL) so concurrent
            // searches and immediate retries don't hammer a broken
            // upstream until the window expires.
            seadex_cache_put_error(anilist_id);
            SeaDexPayload::default()
        }
    }
}

/// Decide whether the current search call needs to make a SeaDex
/// network round-trip. Hashes are required if *either* the config has
/// SeaDex enabled (hardcoded boost) or the compiled CF set contains a
/// `SeaDexBestSpecification` (Custom-Format-driven boost) — so one call
/// serves both paths. Returns the gate flag plus the "hardcoded boost
/// active" flag (suppressed whenever the user has a SeaDex CF, to
/// avoid double-counting).
fn seadex_gates(
    config: &Config,
    cfs: &[CompiledCustomFormat],
) -> (bool /* needs_lookup */, bool /* boost_enabled */) {
    let has_cf = custom_formats::has_seadex_cf(cfs);
    let needs_lookup = config.seadex_enabled || has_cf;
    let boost_enabled = config.seadex_enabled && !has_cf;
    (needs_lookup, boost_enabled)
}

/// True if `info_hash` (non-empty) is in the SeaDex best-hashes set.
///
/// **Both inputs must already be lowercase.** `seadex::best_hashes`
/// populates the set with lowercase strings, and `extract_hash` in
/// `services::nyaa` lowercases every scraped magnet hash at parse
/// time. Enforced by `debug_assert!` so a future caller that forgets
/// fails loudly in tests. The previous version called
/// `info_hash.to_ascii_lowercase()` on every invocation — one
/// allocation per candidate × per CF, which adds up on a batch sweep.
fn is_seadex_match(info_hash: &str, seadex_hashes: &HashSet<String>) -> bool {
    if info_hash.is_empty() || seadex_hashes.is_empty() {
        return false;
    }
    debug_assert!(
        !info_hash.chars().any(|c| c.is_ascii_uppercase()),
        "is_seadex_match: info_hash must be lowercase, got {info_hash:?}"
    );
    seadex_hashes.contains(info_hash)
}

/// Human-readable short label for a series, used in SeaDex lookup log
/// rows. Prefers the English title and falls back to romaji so users
/// browsing the Log Viewer see the same title the Auto Search banner
/// uses.
fn display_title(detail: &AnimeDetail) -> &str {
    if !detail.title_english.is_empty() {
        &detail.title_english
    } else {
        &detail.title_romaji
    }
}

/// Map the config's resolution preference to the bare-number string form that
/// Nyaa search options expect ("480", "720", "1080", "2160").
fn preferred_resolution_search_value(config: &Config) -> String {
    match Resolution::from_str(&config.preferred_resolution) {
        Resolution::R480p => "480".to_string(),
        Resolution::R576p => "576".to_string(),
        Resolution::R720p => "720".to_string(),
        Resolution::R1080p => "1080".to_string(),
        Resolution::R2160p => "2160".to_string(),
        Resolution::Unknown => "1080".to_string(),
    }
}

/// Numbers that look like episode numbers but are actually technical metadata.
#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_sibling_entries_in_pack ──────────────────────────────

    fn pinned_720p_web_tag(
        manual_override: bool,
    ) -> crate::models::episode_tags::EpisodeQualityTag {
        crate::models::episode_tags::EpisodeQualityTag {
            episode_number: 1,
            quality_tag: "WEB-720p".to_string(),
            release_title: "[Group] Show - 01 [WEB-DL 720p].mkv".to_string(),
            release_group: "Group".to_string(),
            state: "completed".to_string(),
            source: "Web".to_string(),
            resolution: "720p".to_string(),
            is_remux: false,
            is_bdmv: false,
            web_kind: "WEBDL".to_string(),
            classification_confidence: 1.0,
            needs_review: false,
            manual_override,
            classification_evidence: String::new(),
            classification_attempted_at: None,
        }
    }

    fn dummy_720p_episode_file(episode_number: i32) -> media::EpisodeFile {
        media::EpisodeFile {
            filename: "[Group] Show - 01 [WEB-DL 720p].mkv".to_string(),
            episode_number,
            season_number: None,
            quality: "720p".to_string(),
            size_bytes: 0,
            size_display: String::new(),
        }
    }

    // Regression: build_upgrade_targets must skip rows the user has pinned
    // via manual override. Otherwise the upgrade sweep selects a "better"
    // release, post-processing replaces the on-disk file, and the
    // manual_override SQL guards on record_grab / update_classification
    // silently drop the tag write — the user loses their pinned file with
    // no audit trail.
    #[test]
    fn build_upgrade_targets_skips_manual_override_rows() {
        let file = dummy_720p_episode_file(1);
        let mut tags = std::collections::HashMap::new();
        tags.insert(1_i32, pinned_720p_web_tag(true));

        let targets = build_upgrade_targets(
            &[file],
            &[1],
            Source::BluRay,
            Resolution::R1080p,
            false,
            false,
            &tags,
        );
        assert!(
            targets.is_empty(),
            "manual_override row should be skipped, got {} target(s)",
            targets.len()
        );
    }

    // Sanity check the regression test: with the same file but
    // manual_override = false, the upgrade target IS produced. Confirms the
    // skip is the new behavior, not an unrelated "everything skips" bug.
    #[test]
    fn build_upgrade_targets_yields_target_when_not_manual_override() {
        let file = dummy_720p_episode_file(1);
        let mut tags = std::collections::HashMap::new();
        tags.insert(1_i32, pinned_720p_web_tag(false));

        let targets = build_upgrade_targets(
            &[file],
            &[1],
            Source::BluRay,
            Resolution::R1080p,
            false,
            false,
            &tags,
        );
        assert_eq!(targets.len(), 1, "auto-classified row should be upgraded");
    }

    // ── #23 — Search override resolver + token append ──────────────────────

    fn series_with_overrides(tokens: &str, user: &str) -> crate::models::series::Series {
        crate::models::series::Series {
            id: 1,
            anilist_id: 1,
            mal_id: None,
            title: String::new(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: String::new(),
            episodes: None,
            season_year: None,
            end_year: None,
            folder_name: String::new(),
            monitor_mode: "future".to_string(),
            allow_upgrades: true,
            custom_query_tokens: tokens.to_string(),
            restrict_to_uploader: user.to_string(),
            cumulative_prior_episodes: 0,
        }
    }

    fn cfg_with_defaults(tokens: &str, user: &str) -> Config {
        Config {
            default_custom_query_tokens: tokens.to_string(),
            default_restrict_to_uploader: user.to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn resolve_overrides_per_series_wins_over_global() {
        let series = series_with_overrides("bd 1080p", "SubsPlease");
        let cfg = cfg_with_defaults("web 720p", "Erai-raws");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(ctx.custom_tokens, "bd 1080p");
        assert_eq!(ctx.restrict_user, "SubsPlease");
    }

    #[test]
    fn resolve_overrides_falls_back_to_global_when_series_blank() {
        let series = series_with_overrides("", "");
        let cfg = cfg_with_defaults("web 720p", "Erai-raws");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(ctx.custom_tokens, "web 720p");
        assert_eq!(ctx.restrict_user, "Erai-raws");
    }

    #[test]
    fn resolve_overrides_per_field_independent_fallback() {
        // One field set, the other blank — blank inherits, set wins.
        let series = series_with_overrides("", "SubsPlease");
        let cfg = cfg_with_defaults("web 720p", "Erai-raws");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(
            ctx.custom_tokens, "web 720p",
            "blank field should inherit global"
        );
        assert_eq!(
            ctx.restrict_user, "SubsPlease",
            "set field should beat global"
        );
    }

    #[test]
    fn resolve_overrides_surfaces_absolute_offset_from_series_row() {
        // #30 — series row carries the cached prior-cour episode count,
        // resolver lifts it verbatim onto the context used by the query
        // sweep.
        let mut series = series_with_overrides("", "");
        series.cumulative_prior_episodes = 47; // e.g. JJK S3 = S1(24) + S2(23)
        let cfg = cfg_with_defaults("", "");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(ctx.absolute_offset, 47);
    }

    #[test]
    fn resolve_overrides_negative_offset_clamped_to_zero() {
        // Defensive: a bad write somewhere upstream mustn't produce
        // negative episode numbers at the filter layer.
        let mut series = series_with_overrides("", "");
        series.cumulative_prior_episodes = -5;
        let cfg = cfg_with_defaults("", "");
        let ctx = resolve_search_overrides_from_row(&series, &cfg);
        assert_eq!(ctx.absolute_offset, 0);
    }

    #[test]
    fn append_tokens_is_noop_when_empty() {
        let qs = vec!["Frieren 01".to_string(), "Frieren - 01".to_string()];
        assert_eq!(append_custom_tokens(qs.clone(), ""), qs);
        assert_eq!(append_custom_tokens(qs.clone(), "   "), qs);
    }

    #[test]
    fn append_tokens_adds_to_each_query() {
        let qs = vec!["Frieren 01".to_string(), "Frieren - 01".to_string()];
        let out = append_custom_tokens(qs, "bd 1080p");
        assert_eq!(
            out,
            vec![
                "Frieren 01 bd 1080p".to_string(),
                "Frieren - 01 bd 1080p".to_string(),
            ]
        );
    }

    // ── #23 follow-up — collapsed query variants when ?u= is active ────

    #[test]
    fn build_queries_full_mode_emits_four_episode_variants() {
        // Regression pin. The full sweep is what runs when no Nyaa
        // uploader filter is set; dropping any of these variants would
        // silently break coverage for uploaders that skip padding,
        // use a specific separator, etc.
        let aliases = vec!["Frieren".to_string()];
        let out = build_queries_from_aliases(&aliases, &SearchTarget::Episode(9), false);
        assert_eq!(
            out.len(),
            4,
            "full-sweep episode target should emit 4 per alias, got {out:?}"
        );
        assert!(out.contains(&"Frieren 9".to_string()));
        assert!(out.contains(&"Frieren - 09".to_string()));
        assert!(out.contains(&"Frieren 09".to_string()));
        assert!(out.contains(&"\"Frieren\" 09".to_string()));
    }

    #[test]
    fn build_queries_collapsed_mode_emits_one_episode_variant() {
        // With /user/<name> scope active, extra variants all return the
        // same uploader's catalog so we drop from 4→1 to cut wall-time.
        let aliases = vec!["Frieren".to_string()];
        let out = build_queries_from_aliases(&aliases, &SearchTarget::Episode(9), true);
        assert_eq!(out, vec!["Frieren 09".to_string()]);
    }

    #[test]
    fn build_queries_collapsed_mode_emits_one_single_variant() {
        let aliases = vec!["Jujutsu Kaisen 0".to_string()];
        let out = build_queries_from_aliases(&aliases, &SearchTarget::Single, true);
        assert_eq!(out, vec!["Jujutsu Kaisen 0".to_string()]);
    }

    #[test]
    fn build_queries_collapsed_scales_with_alias_count() {
        // One variant per alias — the case-insensitive dedupe on
        // `dedupe_strings` collapses romaji/english when they share a
        // lowercase key ("Jujutsu Kaisen" and "JUJUTSU KAISEN"), so a
        // typical three-field AL detail still produces two distinct
        // collapsed queries.
        let aliases = vec![
            "Jujutsu Kaisen".to_string(),
            "JUJUTSU KAISEN".to_string(),
            "呪術廻戦".to_string(),
        ];
        let out = build_queries_from_aliases(&aliases, &SearchTarget::Episode(56), true);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|q| q.ends_with(" 56")));
    }

    // ── SeaDex persistence ───────────────────────────────────────────
    //
    // Use anilist_ids in the 990_000_000+ range so they can't collide
    // with the in-memory `SEADEX_CACHE` global between tests run on the
    // same process. (Tests get their own in-memory SQLite pool, but the
    // process-global LazyLock cache is shared.)

    #[tokio::test]
    async fn seadex_persist_round_trips_through_warm() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        let anilist_id = 990_000_001;
        let mut hashes = HashSet::new();
        hashes.insert("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string());
        let payload = SeaDexPayload {
            hashes,
            candidates: vec![],
        };

        seadex_persist_to_db(&db, anilist_id, &payload).await;
        seadex_warm_cache_from_db(&db).await;

        let cached = seadex_cache_get(anilist_id).expect("warmed entry should be present");
        assert_eq!(cached.hashes.len(), 1);
        assert!(
            cached
                .hashes
                .contains("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
    }

    #[tokio::test]
    async fn seadex_warm_drops_expired_persisted_rows() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        let anilist_id = 990_000_002;
        // Insert a row whose `cached_at` is older than the TTL.
        let stale_cached_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - (SEADEX_CACHE_TTL.as_secs() as i64 + 60);
        sqlx::query(
            "INSERT INTO seadex_lookup_cache (anilist_id, payload_json, cached_at) VALUES (?, ?, ?)",
        )
        .bind(anilist_id)
        .bind(serde_json::to_string(&SeaDexPayload::default()).unwrap())
        .bind(stale_cached_at)
        .execute(&db)
        .await
        .unwrap();

        seadex_warm_cache_from_db(&db).await;

        // The expired row should be evicted from the persistent table
        // and never make it into the in-memory cache.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM seadex_lookup_cache WHERE anilist_id = ?")
                .bind(anilist_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count.0, 0);
        assert!(seadex_cache_get(anilist_id).is_none());
    }

    #[tokio::test]
    async fn seadex_error_cache_is_in_memory_only() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();

        let anilist_id = 990_000_003;
        seadex_cache_put_error(anilist_id);

        // The negative-error path must not write to SQLite — restart
        // should re-probe upstream rather than inherit a "broken" verdict.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM seadex_lookup_cache WHERE anilist_id = ?")
                .bind(anilist_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count.0, 0);
        // But the in-memory cache should hold the (default) negative entry.
        assert!(seadex_cache_get(anilist_id).is_some());
    }

    #[tokio::test]
    async fn seadex_inflight_contains_tracks_registry() {
        // Locks the contract that `prewarm_seadex_negative` relies on
        // when filtering ids: an id with a registered Notify reads as
        // "inflight" and an unregistered one doesn't. Without this
        // gate the prewarm could redundantly issue a request that's
        // already mid-flight on another path.
        let anilist_id = 990_000_004;
        // Sanity: not present at start.
        assert!(!seadex_inflight_contains(anilist_id));
        // Register a fake in-flight entry, then verify the helper sees it.
        {
            let mut map = SEADEX_INFLIGHT.lock().unwrap();
            map.insert(anilist_id, Arc::new(Notify::new()));
        }
        assert!(seadex_inflight_contains(anilist_id));
        // Clean up so other tests aren't affected.
        SEADEX_INFLIGHT.lock().unwrap().remove(&anilist_id);
        assert!(!seadex_inflight_contains(anilist_id));
    }
}
