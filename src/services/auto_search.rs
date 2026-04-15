use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

use regex_lite::Regex;
use sqlx::SqlitePool;

use crate::models::config::Config;
use crate::models::log::LogCategory;
use crate::services::custom_formats::{
    self, CompiledCustomFormat, EvalContext,
};
use crate::services::source::{self, ClassificationResult, Resolution, Source};
use crate::services::{
    anilist::{AnimeDetail, RelatedEntry}, logger, media, nyaa::{self, SearchOptions, SearchResult}, quality, seadex,
};

// ── Pre-compiled regexes for parse_release_numbers ─────────────────────────
static RE_EPISODE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| vec![
    // S01E05 style
    Regex::new(r"s\d{1,2}e(\d{1,4})").unwrap(),
    // E05 / Ep05 / Ep.05 style
    Regex::new(r"(?:^|[\s._\-])e(?:p\.?)?(\d{1,4})(?:v\d)?(?:\s|\.|\[|\(|$)").unwrap(),
    // " - 05" style (common for fansubs)
    Regex::new(r"(?:^|\s)-\s*(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap(),
    // "Episode 05"
    Regex::new(r"episode\s*(\d{1,4})").unwrap(),
]);
static RE_RANGE: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"(?:^|[\s._\-])(\d{1,3})\s*[-~]\s*(\d{1,3})(?:v\d+)?(?:\s|\.|\[|\(|$)").unwrap()
);

// ── Pre-compiled regexes for infer_season_from_title ───────────────────────
static RE_NTH_SEASON: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"(\d+)(?:st|nd|rd|th)\s+season").unwrap()
);
static RE_SEASON_N: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"season\s+(\d+)").unwrap()
);
static RE_PART_COUR: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"(?:part|cour)\s+(\d+)").unwrap()
);

// ── Pre-compiled regexes for parse_release_season ──────────────────────────
static RE_SXXEXX: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"s(\d{1,2})e\d{1,4}").unwrap()
);
static RE_STANDALONE_S: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"(?:^|[\s.\[\(])s(\d{1,2})(?:[\s.\]\)\-]|$)").unwrap()
);
static RE_RELEASE_SEASON_N: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"season\s*(\d+)").unwrap()
);
static RE_RELEASE_NTH_SEASON: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"(\d+)(?:st|nd|rd|th)\s+season").unwrap()
);

#[derive(Debug, Clone)]
pub enum SearchTarget {
    Single,
    Episode(i32),
}

impl SearchTarget {
    /// Build a search target for a user-initiated "search this episode"
    /// action. Collapses to `Single` only when the media is genuinely
    /// single-entry; otherwise stays as `Episode(n)`.
    ///
    /// This exists because the per-episode handlers used to pass
    /// `Episode(n)` unconditionally — and for movies, `matches_target`
    /// then rejected every real release on Nyaa (movie filenames don't
    /// carry episode numbers), leaving Phase 1 empty and triggering the
    /// extended-alias fallback with its looser matching. Collapsing to
    /// `Single` for single-entry media keeps the search on the correct
    /// code path and prevents the fallback from firing in the first
    /// place.
    ///
    /// Rules:
    /// - `MOVIE` → always `Single`. Movies are always single-entry; if
    ///   AniList reports something weird like `episodes: None` or
    ///   `Some(2)`, we still trust the format.
    /// - `SPECIAL` / `OVA` / `ONA` with `episodes == Some(1)` → `Single`.
    ///   These formats are single-entry *in the common case*, but
    ///   multi-episode OVAs (Hellsing Ultimate, LOGH) and multi-episode
    ///   ONAs absolutely exist and their releases DO carry episode
    ///   numbers, so only collapse when AniList explicitly confirms a
    ///   single episode.
    /// - Everything else (TV, TV_SHORT, multi-episode OVA/ONA/SPECIAL,
    ///   or unknown episode count) → `Episode(n)`. TV releases carry
    ///   episode numbers, and for ambiguous formats with unknown episode
    ///   count the safe default is to keep `Episode(n)` — the failure
    ///   mode there is "no results" rather than "wrong series grabbed".
    pub fn for_episode(detail: &AnimeDetail, episode_number: i32) -> Self {
        match detail.format.as_str() {
            "MOVIE" => SearchTarget::Single,
            "SPECIAL" | "OVA" | "ONA" if detail.episodes == Some(1) => SearchTarget::Single,
            _ => SearchTarget::Episode(episode_number),
        }
    }
}

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
    let queries = build_queries(detail, target);
    let aliases = collect_aliases(detail);
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
            let ext_queries = build_queries_from_aliases(&extended, target);
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

    if !preferred_groups.is_empty() {
        let group_queries = build_group_queries(detail, target, &preferred_groups);
        run_queries_interactive(&group_queries, ctx, &mut seen, &mut candidates).await;
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
        );
        // No CF floor on the interactive path — see comment above.
        if let Some(final_score) = apply_cf_seadex_overlay(
            db,
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            i32::MIN,
        )
        .await
        {
            c.score = final_score;
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
    collect_scored_for_target(db, detail, config, target, allow_batch, batch_episode_match, cfs)
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
    };

    // Standard query sweep — picks up any batches that happen to surface
    // on Nyaa page 1 alongside the singles.
    let queries = build_queries(detail, target);
    run_queries(&queries, ctx, &mut seen, &mut candidates).await;

    // Batch-targeted probes — the important addition for this function.
    // Explicit "batch" / "complete" keywords push the Nyaa search toward
    // listings that wouldn't appear on page 1 for a plain title query.
    let batch_queries = quality::batch_probe_queries(&aliases);
    run_queries(&batch_queries, ctx, &mut seen, &mut candidates).await;

    // Preferred-group queries, scoped to batches. Same fallback rule as
    // `collect_scored_for_target`: only fire if no preferred-group hit
    // has surfaced yet.
    let has_preferred_hit = !preferred_groups.is_empty()
        && candidates.iter().any(|c| {
            preferred_groups.iter().any(|g| g.eq_ignore_ascii_case(&c.group))
        });
    if !has_preferred_hit && !preferred_groups.is_empty() {
        let group_queries = build_group_queries(detail, target, &preferred_groups);
        run_queries(&group_queries, ctx, &mut seen, &mut candidates).await;
    }

    // Drop non-batches before the classify/rescore pass so we don't pay
    // the classification cost on candidates we're going to throw away.
    candidates.retain(|c| c.is_batch);

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
        );
        if let Some(final_score) = apply_cf_seadex_overlay(
            db,
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            config.custom_format_minimum_score,
        )
        .await
        {
            c.score = final_score;
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
    let queries = build_queries(detail, target);
    let aliases = collect_aliases(detail);
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
            let ext_queries = build_queries_from_aliases(&extended, target);
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
            preferred_groups.iter().any(|g| g.eq_ignore_ascii_case(&c.group))
        });

    if !has_preferred_hit && !preferred_groups.is_empty() {
        let group_queries = build_group_queries(detail, target, &preferred_groups);
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
            let bd_queries = quality::bd_probe_queries(&aliases);
            run_queries(&bd_queries, ctx, &mut seen, &mut candidates).await;
        }
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
        );
        if let Some(final_score) = apply_cf_seadex_overlay(
            db,
            base,
            &c,
            &classification,
            cfs,
            &seadex_hashes,
            seadex_boost_enabled,
            config.custom_format_minimum_score,
        )
        .await
        {
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
}

/// Same idea, but for the interactive-search helper which has a
/// smaller shared context and no category/batch override.
#[derive(Clone, Copy)]
struct InteractiveQueryCtx<'a> {
    aliases: &'a [String],
    sibling_precompute: &'a SiblingRejectPrecompute,
    preferred_groups: &'a [String],
    preferred_resolution: &'a str,
    target: &'a SearchTarget,
    expected_season: i32,
    /// See the note on `AutoQueryCtx::seadex_hashes`. The interactive
    /// path's consequences for failing the bypass are more severe
    /// than the auto path: `run_queries_interactive` applies
    /// `season_mismatch` *unconditionally*, including for Single
    /// (movie) targets, where the auto path's `matches_target`
    /// skips it. That's why the smol Kizumonogatari II release
    /// vanished from interactive search even though auto search
    /// surfaced it.
    seadex_hashes: &'a HashSet<String>,
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
                user: String::new(),
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
    for query in queries {
        let opts = SearchOptions {
            query: query.clone(),
            category: "1_0".to_string(),
            filter: "0".to_string(),
            user: String::new(),
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
            // Episode check for single-episode targets (allow batches through)
            if let SearchTarget::Episode(target_ep) = ctx.target {
                if !result.is_batch {
                    let parsed = parse_release_numbers(&result.title);
                    if !parsed.is_empty() && !parsed.contains(target_ep) {
                        continue;
                    }
                }
            }
            candidates.push(result);
        }
    }
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

pub fn build_missing_targets(detail: &AnimeDetail, existing_episodes: &[i32]) -> Vec<SearchTarget> {
    let total_eps = detail.episodes.unwrap_or(0);

    if total_eps <= 1 || matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA") {
        return vec![SearchTarget::Single];
    }

    let existing: HashSet<i32> = existing_episodes.iter().copied().collect();
    let mut targets = Vec::new();
    for ep in 1..=total_eps.max(0) {
        if !existing.contains(&ep) {
            targets.push(SearchTarget::Episode(ep));
        }
    }
    targets
}


pub fn build_monitored_targets(detail: &AnimeDetail, existing_episodes: &[i32], monitored_episodes: &[i32]) -> Vec<SearchTarget> {
    if detail.episodes.unwrap_or(0) <= 1 || matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA") {
        if monitored_episodes.is_empty() || monitored_episodes.contains(&1) {
            return vec![SearchTarget::Single];
        }
        return Vec::new();
    }

    let existing: HashSet<i32> = existing_episodes.iter().copied().collect();
    let mut monitored: Vec<i32> = monitored_episodes.to_vec();
    monitored.sort_unstable();
    monitored.dedup();

    monitored
        .into_iter()
        .filter(|ep| !existing.contains(ep))
        .map(SearchTarget::Episode)
        .collect()
}

/// Build upgrade targets: candidate episodes that exist on disk but are below
/// the quality cutoff. These are candidates for automatic quality upgrades.
///
/// Hydration order for each on-disk episode:
/// 1. Structured classification columns on `episode_quality_tags` (written
///    since Phase 1b).
/// 2. Legacy `release_title` column parsed via filename-only classification
///    (for rows grabbed before Phase 1b landed, where the structured cols
///    are empty).
/// 3. On-disk filename + `quality` string, also via filename-only
///    classification (for episodes that have no grab record at all — e.g.
///    pre-existing library files that Ryokan didn't grab itself).
pub fn build_upgrade_targets(
    disk_files: &[media::EpisodeFile],
    candidate_episodes: &[i32],
    cutoff_source: Source,
    cutoff_resolution: Resolution,
    cutoff_is_remux: bool,
    cutoff_is_bdmv: bool,
    quality_tags: &std::collections::HashMap<i32, crate::models::episode_tags::EpisodeQualityTag>,
) -> Vec<(SearchTarget, ClassificationResult)> {
    let candidates: HashSet<i32> = candidate_episodes.iter().copied().collect();
    let cutoff = source::cutoff_classification(
        cutoff_source,
        cutoff_resolution,
        cutoff_is_remux,
        cutoff_is_bdmv,
    );
    let cutoff_rank = cutoff.rank();

    let mut targets = Vec::new();
    for file in disk_files {
        if !candidates.contains(&file.episode_number) {
            continue;
        }
        let existing = resolve_existing_classification(file, quality_tags.get(&file.episode_number));
        // Skip completely unclassified episodes — we have no way to know
        // whether an incoming release would actually be an upgrade.
        if existing.source == Source::Unknown && existing.resolution == Resolution::Unknown {
            continue;
        }
        if existing.rank() < cutoff_rank {
            targets.push((SearchTarget::Episode(file.episode_number), existing));
        }
    }
    targets.sort_by_key(|(t, _)| match t {
        SearchTarget::Episode(n) => *n,
        SearchTarget::Single => 0,
    });
    targets
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

fn build_queries(detail: &AnimeDetail, target: &SearchTarget) -> Vec<String> {
    build_queries_from_aliases(&collect_aliases(detail), target)
}

fn build_queries_from_aliases(aliases: &[String], target: &SearchTarget) -> Vec<String> {
    let mut queries = Vec::new();

    for alias in aliases {
        match target {
            SearchTarget::Single => {
                queries.push(alias.clone());
                queries.push(format!("\"{}\"", alias));
            }
            SearchTarget::Episode(ep) => {
                queries.push(format!("{} {}", alias, ep));
                queries.push(format!("{} - {:02}", alias, ep));
                queries.push(format!("{} {:02}", alias, ep));
                queries.push(format!("\"{}\" {:02}", alias, ep));
            }
        }
    }

    dedupe_strings(queries)
}

/// Primary aliases: romaji, english, native titles only.
pub fn collect_aliases(detail: &AnimeDetail) -> Vec<String> {
    dedupe_strings(vec![
        detail.title_romaji.clone(),
        detail.title_english.clone(),
        detail.title_native.clone(),
    ])
}

/// Distinctive titles of this series' siblings (sequels, prequels, side
/// stories, alternative versions, spin-offs, summaries) — used to reject
/// releases that look MORE like a sibling than the target.
///
/// The motivating bug: auto-searching for Jujutsu Kaisen S1 E6 grabbed a
/// release titled `[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen -
/// 06`, which is actually an S2/S3 arc. The existing season_mismatch()
/// heuristic only catches explicit `S02` / `Season 2` markers; an arc
/// title like "Shimetsu Kaiyuu" slips through. But AniList knows that
/// "Jujutsu Kaisen: Shimetsu Kaiyuu" is a SEQUEL of JJK S1 — so we can
/// use the relation graph to derive the distinctive tokens that, when
/// present in a release filename, mean "this is the sibling, not me".
///
/// Returns sibling titles only where the sibling's normalized title is
/// NOT a substring of any of this target's own aliases (otherwise the
/// sibling title would match the target too — e.g. a prequel sharing
/// the base franchise name is not a useful discriminator). The returned
/// titles are still raw (un-normalized) so the matching logic can
/// re-normalize them the same way it does the release title.
pub fn collect_sibling_aliases(detail: &AnimeDetail, own_aliases: &[String]) -> Vec<String> {
    if detail.id <= 0 || detail.relations.is_empty() {
        return Vec::new();
    }

    // Normalized own-alias set — used to filter out sibling titles that
    // are themselves substrings of one of our own aliases (those would
    // substring-match us too, so they're not distinctive).
    let normalized_own: Vec<String> = own_aliases
        .iter()
        .map(|a| normalize_title(a))
        .filter(|s| !s.is_empty())
        .collect();

    let mut out: Vec<String> = Vec::new();
    for rel in &detail.relations {
        if !rel.media_type.eq_ignore_ascii_case("ANIME") {
            continue;
        }
        if !is_pack_candidate_relation(&rel.relation_type) {
            continue;
        }
        // Consider all three title fields so romaji-only or native-only
        // titles still contribute. The de-dup below squashes repeats.
        for raw in [
            rel.title_english.as_str(),
            rel.title_romaji.as_str(),
            rel.title_native.as_str(),
        ] {
            if raw.is_empty() {
                continue;
            }
            let normalized = normalize_title(raw);
            // Need ≥ 2 tokens for the sibling title to be a meaningful
            // discriminator — a single token is too generic and will
            // false-positive on unrelated releases that happen to share
            // a common word.
            if normalized.split_whitespace().count() < 2 {
                continue;
            }
            // Skip sibling titles whose normalized form is a substring
            // of one of our own aliases — those can't tell us apart
            // from the target.
            if normalized_own.iter().any(|own| own.contains(&normalized)) {
                continue;
            }
            out.push(raw.to_string());
        }
    }
    dedupe_strings(out)
}

/// Precomputed normalized token sets for the own-alias and sibling-alias
/// lists used by [`sibling_match_rejects`]. Built once per target sweep
/// (per call to `find_all_for_target` / `collect_scored_for_target` /
/// `collect_scored_batches_for_target`) and reused across every release
/// candidate the sweep checks against the target, instead of re-running
/// `normalize_title` + `token_set` on the same alias strings ~50×
/// (candidates) per target. Pure perf hoist — the rejection semantics
/// are identical to the prior per-call implementation.
#[derive(Debug, Clone, Default)]
pub struct SiblingRejectPrecompute {
    /// Token sets for own aliases. Used to find the best target-alias
    /// overlap with any release — a sibling only wins if it beats this
    /// number strictly.
    own_token_sets: Vec<HashSet<String>>,
    /// Sibling entries as `(normalized_title, token_set)` pairs. The
    /// normalized title is kept alongside its token set so the
    /// contiguous-substring fallback has a stable, deterministic string
    /// to match against (the old implementation rebuilt this from
    /// `HashSet::iter()` per call, which is nondeterministic order and
    /// would silently misbehave on contiguous-substring checks).
    siblings: Vec<(String, HashSet<String>)>,
}

impl SiblingRejectPrecompute {
    pub fn build(own_aliases: &[String], sibling_aliases: &[String]) -> Self {
        let own_token_sets = own_aliases
            .iter()
            .map(|a| token_set(&normalize_title(a)))
            .collect();
        let siblings = sibling_aliases
            .iter()
            .filter_map(|s| {
                let normalized = normalize_title(s);
                let tokens = token_set(&normalized);
                if tokens.is_empty() {
                    None
                } else {
                    Some((normalized, tokens))
                }
            })
            .collect();
        Self {
            own_token_sets,
            siblings,
        }
    }

}

/// Reject a release when it looks MORE like one of our siblings than
/// it does like us. The check compares token overlap: if any sibling
/// alias shares strictly more tokens with the release than the best
/// target alias does, the release is for the sibling.
///
/// Returns `true` to reject, `false` to keep.
///
/// Called from `matches_target` and the interactive-search path. Both
/// are guarded by an upstream basic alias-match, so by the time we get
/// here the release already passes the "could plausibly be us" gate —
/// the sibling check is the last defense against "plausibly us" also
/// being "more plausibly a sibling".
fn sibling_match_rejects(
    normalized_release: &str,
    normalized_release_tokens: &HashSet<String>,
    precompute: &SiblingRejectPrecompute,
) -> bool {
    if precompute.siblings.is_empty() {
        return false;
    }

    // Best token overlap COUNT between release and any of our own aliases.
    // Using absolute overlap count (not ratio) so a sibling with 4 matching
    // tokens beats a target alias with 2 matching tokens even if the target
    // alias has fewer tokens overall.
    let best_own_overlap: usize = precompute
        .own_token_sets
        .iter()
        .map(|tokens| normalized_release_tokens.intersection(tokens).count())
        .max()
        .unwrap_or(0);

    for (normalized_sibling, sibling_tokens) in &precompute.siblings {
        let sibling_overlap = normalized_release_tokens
            .intersection(sibling_tokens)
            .count();
        // Strictly greater: a tie means both the target and the sibling
        // match equally well, which is the normal case for a release
        // like "Jujutsu Kaisen - 06" where sibling "Jujutsu Kaisen 2nd
        // Season" also overlaps on {jujutsu, kaisen}. Only reject when
        // the sibling picks up EXTRA tokens that the target doesn't.
        if sibling_overlap > best_own_overlap {
            // Also require that the sibling's entire normalized title
            // is either a contiguous substring of the release or that
            // ALL of its tokens appear in the release. This prevents
            // freak two-token overlaps ("side story" + some other
            // common fragment) from tripping the rejection.
            let all_tokens_present = sibling_tokens
                .iter()
                .all(|t| normalized_release_tokens.contains(t));
            if all_tokens_present || normalized_release.contains(normalized_sibling.as_str()) {
                return true;
            }
        }
    }
    false
}

/// Extended aliases: synonyms + decomposed sub-phrases from compound titles.
/// Only used as a fallback when primary aliases don't find results.
pub fn collect_extended_aliases(detail: &AnimeDetail) -> Vec<String> {
    let primary = collect_aliases(detail);
    let mut extra = Vec::new();

    // Add AniList synonyms.
    extra.extend(detail.synonyms.iter().cloned());

    // Decompose all titles (primary + synonyms) into sub-phrases.
    // Nyaa releases often use just the subtitle portion
    // (e.g. "Steel Ball Run" from "JoJo's Bizarre Adventure: Part 7–Steel Ball Run").
    let all_titles: Vec<String> = primary.iter().chain(extra.iter()).cloned().collect();
    for title in &all_titles {
        for segment in split_title_segments(title) {
            extra.push(segment);
        }
    }

    // Return only the NEW aliases (not already in primary).
    let primary_lower: HashSet<String> = primary.iter().map(|s| s.to_lowercase()).collect();
    dedupe_strings(extra)
        .into_iter()
        .filter(|s| !primary_lower.contains(&s.to_lowercase()))
        .collect()
}

/// Split a compound title on common delimiters and return meaningful segments.
/// Filters out segments that are too short or too generic to be useful search
/// terms.
///
/// Segments are used both as Nyaa search queries AND as matching aliases
/// inside `matches_target`, which means an over-generic segment can
/// substring-match unrelated shows on Nyaa and cause a completely wrong
/// grab. A single-word subtitle (especially a common English word or
/// hyphenated phrase) is almost always ambiguous — it will substring-match
/// any release that happens to contain the word, regardless of whether
/// that release is for this show or an unrelated one with the same word
/// in its name.
///
/// The 2-token minimum is the cheap defense: segments with only one
/// whitespace-separated token are rejected, regardless of length, because
/// they can't be trusted to uniquely identify a show. Segments with 2+
/// tokens remain — those are specific enough that substring-matching them
/// against an unrelated release is vanishingly unlikely.
fn split_title_segments(title: &str) -> Vec<String> {
    // Normalize various dash types to a common delimiter for splitting.
    let normalized = title
        .replace(['–', '—'], "|")  // en dash and em dash
        .replace(": ", "|") // colon+space (keep "Re:Zero" intact)
        .replace(" - ", "|");

    let mut segments = Vec::new();
    for part in normalized.split('|') {
        let trimmed = part.trim();
        // Skip segments that are too short or just "Part N" / "Season N".
        if trimmed.len() < 5 {
            continue;
        }
        if trimmed.eq_ignore_ascii_case(title.trim()) {
            continue;
        }
        // Require at least 2 whitespace-separated tokens. Single-word
        // segments are too generic to use as matching aliases: they can
        // substring-match any release title that happens to contain the
        // word (see doc comment above for the Kizumonogatari / Gundam
        // Iron-Blooded Orphans incident).
        if trimmed.split_whitespace().count() < 2 {
            continue;
        }
        // Skip pure numbering like "Part 7", "Season 2", "2nd Season".
        let lower = trimmed.to_lowercase();
        if lower.starts_with("part ") && lower.len() < 10 {
            continue;
        }
        if lower.starts_with("season ") && lower.len() < 12 {
            continue;
        }
        if lower.ends_with(" season") && lower.len() < 14 {
            continue;
        }
        segments.push(trimmed.to_string());
    }
    segments
}

pub fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(trimmed.to_string());
        }
    }
    out
}

pub fn matches_target(
    title: &str,
    aliases: &[String],
    sibling_precompute: &SiblingRejectPrecompute,
    target: &SearchTarget,
    expected_season: i32,
    allow_batch_episode: bool,
) -> bool {
    let normalized_title = normalize_title(title);
    let title_tokens = token_set(&normalized_title);

    let alias_match = aliases.iter().any(|alias| {
        let normalized_alias = normalize_title(alias);
        normalized_title.contains(&normalized_alias)
            || token_overlap_ratio(&title_tokens, &token_set(&normalized_alias)) >= 0.6
    });

    if !alias_match {
        return false;
    }

    // Sibling rejection: if the release looks more like a sequel /
    // prequel / side story than it looks like us, reject. See the
    // JJK S1→S3 case in the `collect_sibling_aliases` docstring.
    if sibling_match_rejects(&normalized_title, &title_tokens, sibling_precompute) {
        return false;
    }

    match target {
        SearchTarget::Single => true,
        SearchTarget::Episode(target_ep) => {
            // Season check: reject if release has an explicit season that doesn't match
            if season_mismatch(title, expected_season) {
                return false;
            }

            let parsed = parse_release_numbers(title);
            if parsed.is_empty() {
                return false;
            }
            // Reject releases with 3+ episode numbers (batch/multi-episode)
            // unless the caller explicitly allows batch-to-episode matching
            // (used for quality upgrade searches where BD season packs are the
            // only source for higher-quality individual episodes).
            if !allow_batch_episode && parsed.len() > 2 {
                return false;
            }
            parsed.contains(target_ep)
        }
    }
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
#[derive(Default, Clone)]
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
static SEADEX_CACHE: LazyLock<StdMutex<HashMap<i64, (Instant, SeaDexPayload)>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn seadex_cache_get(anilist_id: i64) -> Option<SeaDexPayload> {
    let cache = SEADEX_CACHE.lock().ok()?;
    let (fetched_at, payload) = cache.get(&anilist_id)?;
    if fetched_at.elapsed() < SEADEX_CACHE_TTL {
        Some(payload.clone())
    } else {
        None
    }
}

fn seadex_cache_put(anilist_id: i64, payload: SeaDexPayload) {
    if let Ok(mut cache) = SEADEX_CACHE.lock() {
        cache.insert(anilist_id, (Instant::now(), payload));
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

            // Fetch each usable torrent's view page so we have a real
            // SearchResult to inject into the candidate list. This is
            // what keeps SeaDex-curated releases discoverable even when
            // the text-query sweep can't find them by title. Failures
            // are non-fatal: we log and move on to the next torrent.
            let opts_for_score = nyaa::SearchOptions {
                query: series_title.to_string(),
                category: "1_0".to_string(),
                filter: "0".to_string(),
                user: String::new(),
                preferred_groups: preferred_groups.to_vec(),
                preferred_resolution: preferred_resolution.to_string(),
                prefer_subs,
            };
            let mut candidates = Vec::new();
            for torrent in entry.torrents.iter() {
                if !seadex::is_usable(torrent, &entry.notes) {
                    continue;
                }
                let view_url = seadex::to_nyaa_view_url(torrent);
                match nyaa::fetch_view_result(view_url, &opts_for_score).await {
                    Ok(result) => {
                        tracing::debug!(
                            "seadex: injected curated candidate from view url={} title={:?} hash={}",
                            view_url,
                            result.title,
                            result.info_hash
                        );
                        candidates.push(result);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "seadex: failed to fetch view page for {}: {}",
                            view_url,
                            e
                        );
                        logger::warn(
                            db,
                            LogCategory::AutoSearch,
                            &format!("SeaDex view-page fetch failed for {series_title}"),
                            &format!("url={view_url}, error={e}"),
                        )
                        .await;
                    }
                }
            }
            let payload = SeaDexPayload { hashes, candidates };
            seadex_cache_put(anilist_id, payload.clone());
            payload
        }
        Ok(None) => {
            tracing::debug!(
                "seadex: releases.moe has no entry for anilist_id={anilist_id}"
            );
            logger::debug(
                db,
                LogCategory::AutoSearch,
                &format!("SeaDex has no entry for {series_title}"),
                &format!("anilist_id={anilist_id}"),
            )
            .await;
            // Cache the "no entry" result so we don't re-hit releases.moe
            // for the same anilist_id within the TTL window. Err paths
            // are intentionally NOT cached — transient failures should
            // retry on the next call.
            let payload = SeaDexPayload::default();
            seadex_cache_put(anilist_id, payload.clone());
            payload
        }
        Err(e) => {
            tracing::warn!(
                "seadex: releases.moe lookup failed for anilist_id={anilist_id}: {e}"
            );
            logger::warn(
                db,
                LogCategory::AutoSearch,
                &format!("SeaDex lookup failed for {series_title}"),
                &format!("anilist_id={anilist_id}, error={e}"),
            )
            .await;
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

/// Apply the Custom Format + SeaDex overlay to a base score.
///
/// Returns `Some(final_score)` if the candidate survives the CF
/// minimum-score floor, or `None` if it should be dropped. The SeaDex
/// score bump is suppressed whenever the compiled CF set contains a
/// `SeaDexBestSpecification` — the user has taken ownership of that
/// number and double-counting would be a silent regression.
///
/// On the way through, emits one `LogCategory::Scoring` debug row per
/// candidate with a CF-aware breakdown line (plan §6.3). Dropped
/// candidates are logged too so the user can introspect "why did this
/// candidate get cut from the results" in addition to "why did X win."
#[allow(clippy::too_many_arguments)]
async fn apply_cf_seadex_overlay(
    db: &SqlitePool,
    base: i32,
    result: &SearchResult,
    classification: &ClassificationResult,
    cfs: &[CompiledCustomFormat],
    seadex_hashes: &HashSet<String>,
    seadex_boost_enabled: bool,
    minimum_score: i32,
) -> Option<i32> {
    let ctx = EvalContext {
        result,
        classification,
        seadex_hashes,
    };
    // Use the breakdown variant so the log line can name which CFs
    // contributed. Per plan §6.3, production scoring normally uses the
    // scalar `total_cf_score`; the cost of the `Vec<(String, i32)>`
    // allocation is absorbed here because we're about to log anyway.
    let (cf, breakdown) = custom_formats::total_cf_score_with_breakdown(cfs, &ctx);
    let seadex_bonus = if seadex_boost_enabled
        && !result.info_hash.is_empty()
        && seadex_hashes.contains(&result.info_hash.to_ascii_lowercase())
    {
        seadex::SEADEX_SCORE_BOOST
    } else {
        0
    };

    let below_floor = cf < minimum_score;
    let final_score = base + cf + seadex_bonus;

    let detail = format_scoring_detail(base, cf, &breakdown, seadex_bonus, final_score, below_floor);
    logger::debug(db, LogCategory::Scoring, &result.title, &detail).await;

    if below_floor {
        None
    } else {
        Some(final_score)
    }
}

/// Build the structured scoring detail string that lands in the
/// `logs.detail` column. Factored out of `apply_cf_seadex_overlay` so
/// the format is in one place and unit-testable. Matches the shape
/// documented in plan §6.3:
///
/// `base=85, cf=+420 [10bit x265 +200, FLAC audio +120, Preferred Groups: MTBB +100], seadex=0, final=505`
///
/// Negative contributions include the sign. An empty breakdown drops
/// the bracket section entirely ("cf=+0" with nothing inside would be
/// noisy). Candidates dropped by the CF minimum-score floor get a
/// trailing ` DROPPED(floor=N)` marker so log readers can tell filtered
/// candidates apart from surviving ones.
fn format_scoring_detail(
    base: i32,
    cf: i32,
    breakdown: &[(String, i32)],
    seadex_bonus: i32,
    final_score: i32,
    below_floor: bool,
) -> String {
    let cf_section = if breakdown.is_empty() {
        format!("cf={cf:+}")
    } else {
        let parts: Vec<String> = breakdown
            .iter()
            .map(|(name, score)| format!("{name} {score:+}"))
            .collect();
        format!("cf={:+} [{}]", cf, parts.join(", "))
    };
    let mut out = format!(
        "base={base}, {cf_section}, seadex={seadex_bonus}, final={final_score}"
    );
    if below_floor {
        out.push_str(" DROPPED(below minimum_score floor)");
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn rescore_for_auto_search(
    result: &SearchResult,
    classification: &ClassificationResult,
    config: &Config,
    aliases: &[String],
    target: &SearchTarget,
    expected_season: i32,
    is_finished: bool,
    finished_mode: quality::FinishedSeriesMode,
    preferred_source: Source,
    preferred_resolution: Resolution,
    cutoff_source: Source,
    cutoff_resolution: Resolution,
) -> i32 {
    let mut score = result.score;
    let lower = result.title.to_lowercase();
    let normalized_title = normalize_title(&result.title);
    let title_tokens = token_set(&normalized_title);

    let best_overlap = aliases.iter().map(|alias| {
        let normalized_alias = normalize_title(alias);
        if normalized_title.contains(&normalized_alias) {
            1.0
        } else {
            token_overlap_ratio(&title_tokens, &token_set(&normalized_alias))
        }
    }).fold(0.0f32, f32::max);
    score += (best_overlap * 40.0) as i32;

    // Season mismatch penalty (explicit season markers like S03, "3rd Season")
    if season_mismatch(&result.title, expected_season) {
        score -= 100;
    }

    match target {
        SearchTarget::Single => {
            if lower.contains("movie") || lower.contains("special") || lower.contains("ova") {
                score += 8;
            }
            if result.is_batch {
                score -= 5;
            }
        }
        SearchTarget::Episode(ep) => {
            if result.is_batch {
                score -= 20;
            } else {
                score += 10;
            }
            if parse_release_numbers(&result.title).contains(ep) {
                score += 40;
            }
        }
    }

    score += quality::preferred_group_bonus(&result.group, &quality::parse_group_list(&config.preferred_groups));

    // Classification-aware quality scoring.
    score += source::score_classification(
        classification,
        preferred_source,
        preferred_resolution,
        cutoff_source,
        cutoff_resolution,
    );

    // For finished series with BD preference, give BD releases a significant boost.
    if is_finished
        && finished_mode == quality::FinishedSeriesMode::PreferBd
        && classification.source == Source::BluRay
    {
        score += 35;
    }

    score
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
fn is_noise_number(n: i32) -> bool {
    matches!(n, 480 | 576 | 720 | 1080 | 2160 | 264 | 265)
        || (1900..=2100).contains(&n)
}

pub fn parse_release_numbers(title: &str) -> HashSet<i32> {
    let lower = title.to_lowercase();
    let mut numbers = HashSet::new();

    // Strip bracketed content first to avoid matching metadata like [1080p] or (2024)
    let stripped = {
        let mut out = String::with_capacity(lower.len());
        let mut depth = 0i32;
        for ch in lower.chars() {
            match ch {
                '[' | '(' | '{' => depth += 1,
                ']' | ')' | '}' => depth = (depth - 1).max(0),
                _ if depth > 0 => continue,
                _ => out.push(ch),
            }
        }
        out
    };

    for re in RE_EPISODE_PATTERNS.iter() {
        for caps in re.captures_iter(&stripped) {
            if let Some(value) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
                if !is_noise_number(value) {
                    numbers.insert(value);
                }
            }
        }
    }

    // Range pattern for batch detection (e.g. "01-12", "01~24")
    // Only add range numbers, not used as the sole episode match.
    if let Some(caps) = RE_RANGE.captures(&stripped) {
        let start = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()).unwrap_or(0);
        let end = caps.get(2).and_then(|m| m.as_str().parse::<i32>().ok()).unwrap_or(0);
        if start > 0 && end >= start && end - start <= 200 && !is_noise_number(start) && !is_noise_number(end) {
            for value in start..=end {
                numbers.insert(value);
            }
        }
    }

    numbers
}

pub fn normalize_title(input: &str) -> String {
    let lower = input.to_lowercase();
    let mut cleaned = String::with_capacity(lower.len());
    let mut in_brackets = 0i32;

    for ch in lower.chars() {
        match ch {
            '[' | '(' | '{' => in_brackets += 1,
            ']' | ')' | '}' => in_brackets = (in_brackets - 1).max(0),
            _ if in_brackets > 0 => continue,
            _ if ch.is_alphanumeric() || ch.is_whitespace() => cleaned.push(ch),
            _ => cleaned.push(' '),
        }
    }

    cleaned
        .split_whitespace()
        .filter(|token| !matches!(*token, "1080p" | "720p" | "2160p" | "webrip" | "web" | "bluray" | "aac" | "hevc" | "x265" | "x264" | "dual" | "audio" | "multisub"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn token_set(value: &str) -> HashSet<String> {
    value
        .split_whitespace()
        .filter(|token| token.len() > 1)
        .map(|token| token.to_string())
        .collect()
}

pub fn token_overlap_ratio(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let common = a.intersection(b).count() as f32;
    common / b.len() as f32
}

/// Infer the season number from AniList detail titles.
/// Returns 0 if no season indicator is found (treated as season 1 during matching).
pub fn infer_season_from_detail(detail: &AnimeDetail) -> i32 {
    let aliases = collect_aliases(detail);
    for alias in &aliases {
        let s = infer_season_from_title(alias);
        if s > 0 {
            return s;
        }
    }
    0
}

fn infer_season_from_title(title: &str) -> i32 {
    let lower = title.to_lowercase();

    // "2nd Season", "3rd Season", etc.
    if let Some(caps) = RE_NTH_SEASON.captures(&lower) {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            return n;
        }
    }

    // "Season 2", "Season 3", etc.
    if let Some(caps) = RE_SEASON_N.captures(&lower) {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            return n;
        }
    }

    // " Part 2", " Cour 2" — sometimes used as season aliases
    if let Some(caps) = RE_PART_COUR.captures(&lower) {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            if n >= 2 {
                return n;
            }
        }
    }

    0
}

/// Parse the season number from a release title.
/// Returns 0 if no season indicator is found.
pub fn parse_release_season(title: &str) -> i32 {
    let lower = title.to_lowercase();

    // S01E05, S02E03, etc.
    if let Some(caps) = RE_SXXEXX.captures(&lower) {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            return n;
        }
    }

    // Standalone "S2", "S3" (not part of resolution like "S01E01")
    if let Some(caps) = RE_STANDALONE_S.captures(&lower) {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            if n > 0 && n <= 30 {
                return n;
            }
        }
    }

    // "Season 2", "Season 3"
    if let Some(caps) = RE_RELEASE_SEASON_N.captures(&lower) {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            return n;
        }
    }

    // "2nd Season", "3rd Season"
    if let Some(caps) = RE_RELEASE_NTH_SEASON.captures(&lower) {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            return n;
        }
    }

    0
}

// ── Pre-compiled regexes for extract_part_number ───────────────────────────
//
// `extract_part_number` recovers the "which entry in a multi-part release"
// number from an AniList title so the selective-download path can match
// it against per-file episode numbers inside a megapack. This is distinct
// from `infer_season_from_detail`, which is about season/cour indexing
// for the *query* sweep. A movie trilogy like Kizumonogatari I/II/III
// has no season at all, just parts.
static RE_PART_N: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"(?i)\b(?:part|chapter|movie|film)\s*(\d{1,2})\b").unwrap()
);

/// Roman numeral at a word boundary, II–IX only. Matches at end of
/// string or before common separators (`:` for subtitle, space, `-`)
/// so titles like "Kizumonogatari II: Nekketsu-hen" and "Rebuild of
/// Evangelion III" both resolve cleanly.
///
/// **Single-letter Romans (`I`, `V`, `X`) are deliberately excluded.**
/// Matching bare `I` would fire on any anime title containing the
/// English pronoun — "I Want to Eat Your Pancreas", "I, Robot" — and
/// resolve them as part 1, causing `pick_by_part_number` to narrow a
/// megapack to the wrong file. Bare `V` and `X` carry the same risk
/// ("V for Vendetta", "X/1999"). The tradeoff is that trilogy first
/// entries titled "Kizumonogatari I" no longer narrow to E01 inside a
/// megapack — they fall through to the full pack instead, which is
/// fine because users rarely grab a trilogy's opening chapter in
/// isolation. Explicit markers like "Part 1" / "Chapter 1" still
/// work via [`RE_PART_N`].
static RE_ROMAN_PART: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"(?i)\b(ii{1,2}|iv|vi{1,3}|ix)\b(?:\s*[:\-]|$|\s)").unwrap()
);

/// Extract the "part number" from an AniList detail's titles. Returns
/// `None` when the title carries no such marker — the common case for
/// single-film works and standalone TV seasons.
///
/// This is used by the selective-file download path: when the user
/// grabs a megapack (e.g. the smol Monogatari pack containing Kizumo
/// I, II, III as separate files), we need to know "the target is part
/// 2" so `pick_wanted_file_indices` can match it against the E02 file.
///
/// Matching order (first hit wins):
///   1. Explicit `Part N` / `Chapter N` / `Movie N` / `Film N`
///   2. Roman numeral I–X at a word boundary
///
/// We check all three alias titles (romaji, english, native). Native
/// is unlikely to fire the English regexes but it's cheap to include.
pub fn extract_part_number(detail: &AnimeDetail) -> Option<i32> {
    let titles = [
        detail.title_english.as_str(),
        detail.title_romaji.as_str(),
        detail.title_native.as_str(),
    ];
    for title in titles.iter().filter(|t| !t.is_empty()) {
        if let Some(caps) = RE_PART_N.captures(title) {
            if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
                if (1..=20).contains(&n) {
                    return Some(n);
                }
            }
        }
        if let Some(caps) = RE_ROMAN_PART.captures(title) {
            if let Some(m) = caps.get(1) {
                let n = roman_to_int(m.as_str());
                if (1..=10).contains(&n) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Convert a Roman numeral in the I–X range to its integer value.
/// Returns 0 on anything outside that range so the caller's bounds
/// check can cleanly reject it.
fn roman_to_int(s: &str) -> i32 {
    match s.to_ascii_uppercase().as_str() {
        "I" => 1,
        "II" => 2,
        "III" => 3,
        "IV" => 4,
        "V" => 5,
        "VI" => 6,
        "VII" => 7,
        "VIII" => 8,
        "IX" => 9,
        "X" => 10,
        _ => 0,
    }
}

/// Given the file list of a multi-entry pack torrent and the AniList
/// detail of the user's target, return the indices of the files that
/// correspond to the target. Returns `None` when we can't narrow the
/// selection — the safe default is "keep everything" rather than
/// guessing wrong and skipping the one file the user wanted.
///
/// Two narrowing strategies are tried in order:
///
/// 1. **Part number** via [`extract_part_number`]. Handles trilogies
///    and multi-part OVAs where AniList titles end in "II", "III",
///    "Part 2", etc. File selection is done by parsing episode-like
///    numbers from each filename (via [`parse_release_numbers`]) and
///    keeping files whose numbers contain the target part.
///
/// 2. **Positive subtitle match** via [`extract_season_subtitle`].
///    Handles franchise megapacks where the target itself carries a
///    distinguishing suffix after `:` or ` - ` — e.g. "JoJo's Bizarre
///    Adventure: Stardust Crusaders" inside a JoJo S1–S5 pack. The
///    filename must contain the normalized subtitle.
///
/// Non-media files (NFO, TXT, subtitles) are ignored so they don't
/// dilute the selection. In both strategies, the result must fall
/// within the target's expected episode count (×1.5 + 2 for slack)
/// or we return `None` — guards against positive matches that
/// accidentally sweep in bonus features or siblings that share a
/// subtitle substring.
///
/// Note: **franchise roots without their own subtitle** (e.g. JoJo S1
/// = "JoJo's Bizarre Adventure") are intentionally NOT narrowed here.
/// They're handled by the higher-level multi-series pack detection
/// path which downloads the full pack and auto-adds detected sibling
/// entries to the library instead — a cleaner answer than
/// filename-based negative matching, which is prone to partial
/// coverage when AniList relations don't include every sibling.
pub fn pick_wanted_file_indices(
    filenames: &[String],
    detail: &AnimeDetail,
) -> Option<Vec<usize>> {
    if let Some(part) = extract_part_number(detail) {
        if let Some(ids) = pick_by_part_number(filenames, part, detail) {
            return Some(ids);
        }
    }
    if let Some(subtitle) = extract_season_subtitle(detail) {
        if let Some(ids) = pick_by_subtitle_include(filenames, &subtitle, detail) {
            return Some(ids);
        }
    }
    None
}

/// Narrow a megapack to the files that correspond to the target's
/// part number.
///
/// **Assumes 1 part = 1 episode number in the filename.** Works for
/// the canonical smol Monogatari pack (Kizumonogatari I/II/III land
/// as S09E01/S09E02/S09E03) and similar per-episode layouts. Breaks
/// for releases where a single "part" spans multiple files — e.g.
/// multi-file BDMV rips of a single film, or "Part 2 E13-E24" —
/// because `parse_release_numbers(filename).contains(&part)` then
/// matches the wrong files entirely. Rebuild of Evangelion 1.0/2.0/3.0
/// happens to fall through to `None` for a different reason:
/// `parse_release_numbers` doesn't capture `2.22`-style decimal parts,
/// so the match set is empty and the caller keeps the whole pack —
/// which is the safe outcome.
///
/// Returns `None` when:
/// - no files match (keep-everything is safer than picking wrong),
/// - every file matches (nothing was actually narrowed),
/// - the match set exceeds the target's expected episode count (guards
///   against a `part=1` query sweeping in every episode in a 24-ep TV
///   season when the target is actually a 2-ep OVA).
fn pick_by_part_number(
    filenames: &[String],
    part: i32,
    detail: &AnimeDetail,
) -> Option<Vec<usize>> {
    let mut matches: Vec<usize> = Vec::new();
    let mut media_count = 0usize;
    for (idx, name) in filenames.iter().enumerate() {
        if !is_media_filename(name) {
            continue;
        }
        media_count += 1;
        if parse_release_numbers(name).contains(&part) {
            matches.push(idx);
        }
    }
    if matches.is_empty() || matches.len() >= media_count {
        return None;
    }
    if !within_expected_episode_count(matches.len(), detail) {
        return None;
    }
    Some(matches)
}

fn pick_by_subtitle_include(
    filenames: &[String],
    subtitle: &str,
    detail: &AnimeDetail,
) -> Option<Vec<usize>> {
    let needle = normalize_subtitle(subtitle);
    if needle.is_empty() {
        return None;
    }
    let mut matches: Vec<usize> = Vec::new();
    let mut media_count = 0usize;
    for (idx, name) in filenames.iter().enumerate() {
        if !is_media_filename(name) {
            continue;
        }
        media_count += 1;
        if normalize_subtitle(name).contains(&needle) {
            matches.push(idx);
        }
    }
    if matches.is_empty() || matches.len() >= media_count {
        return None;
    }
    if !within_expected_episode_count(matches.len(), detail) {
        return None;
    }
    Some(matches)
}

/// Sanity-cap the narrowed selection against the target's expected
/// episode count. Rejects selections that are implausibly larger than
/// the target's own season (×1.5 slack plus 2 for rounding / bonus
/// features / BD extras). Without this guard, a positive subtitle
/// match could accidentally sweep in files for a longer sibling that
/// shares a subtitle substring, and the selective log line would
/// mask the overshoot as a successful narrowing.
///
/// Shows that are still airing report `episodes: None` from AniList;
/// in that case `effective_episode_count()` falls back to
/// `nextAiringEpisode - 1`, which is 0 during the week-0 pre-airing
/// window. Returning `true` unconditionally for 0-count targets keeps
/// the cap disabled for airing shows and lets the strategy's own
/// `matches.len() < media_count` guard carry the safety load.
fn within_expected_episode_count(matches_len: usize, detail: &AnimeDetail) -> bool {
    within_episode_slack(matches_len, detail.effective_episode_count())
}

/// Raw-count version of [`within_expected_episode_count`]. Shared with
/// [`detect_sibling_entries_in_pack`], where the "expected" value comes
/// from a `RelatedEntry` card that doesn't carry `next_airing_episode`
/// and therefore can't use `effective_episode_count`.
fn within_episode_slack(matches_len: usize, expected: i32) -> bool {
    if expected <= 0 {
        return true;
    }
    let slack = (expected as f32 * 1.5).ceil() as usize + 2;
    matches_len <= slack
}

/// Extract a season "subtitle" — the trailing portion of the target's
/// title after a delimiter like `: ` or ` - `. For example:
/// * "JoJo no Kimyou na Bouken: Stardust Crusaders" → `Some("Stardust Crusaders")`
/// * "Fate/stay night: Unlimited Blade Works" → `Some("Unlimited Blade Works")`
/// * "Fullmetal Alchemist: Brotherhood" → `None` (single-token subtitle)
/// * "JoJo's Bizarre Adventure" → `None` (no delimiter)
/// * "Monogatari Series: Second Season" → `None` (generic ordinal)
///
/// Prefers the English title, falls back to romaji. Rejects single-token
/// subtitles because they substring-match too aggressively, and rejects
/// pure ordinal "Nth Season" phrasings because release filenames almost
/// always carry "S02" / "2nd" rather than the English rendering, so
/// matching on them yields zero hits and forces the full-pack fallback
/// anyway.
pub fn extract_season_subtitle(detail: &AnimeDetail) -> Option<String> {
    let titles = [
        detail.title_english.as_str(),
        detail.title_romaji.as_str(),
    ];
    for title in titles.iter().filter(|t| !t.is_empty()) {
        if let Some(sub) = trailing_subtitle_of(title) {
            return Some(sub);
        }
    }
    None
}

fn trailing_subtitle_of(title: &str) -> Option<String> {
    // Normalize CJK/en/em dashes and colon-space to a common delimiter.
    // Preserving "Re:" / "Fate/" (no space after) means "Re:Zero kara..."
    // and "Fate/stay night" stay intact and only the trailing "`: Sub`"
    // portion gets split off.
    let normalized = title
        .replace(['–', '—'], "|")
        .replace(": ", "|")
        .replace('：', "|")
        .replace(" - ", "|");
    // Take the LAST segment so "A: B: C" resolves to the innermost "C".
    let last = normalized.rsplit('|').next()?.trim();
    if last.is_empty() || last.eq_ignore_ascii_case(title.trim()) {
        return None;
    }
    // Require ≥ 2 whitespace tokens. Single-word subtitles like
    // "Brotherhood" are too generic to reliably narrow a filename list
    // without false positives on unrelated entries in the same pack.
    if last.split_whitespace().count() < 2 {
        return None;
    }
    let lower = last.to_ascii_lowercase();
    if is_generic_season_subtitle(&lower) {
        return None;
    }
    Some(last.to_string())
}

/// Returns true for subtitle phrases that are pure ordinal/numeric
/// season markers (e.g. "Second Season", "2nd Season", "Part 3"). These
/// are rejected by [`extract_season_subtitle`] because release filenames
/// overwhelmingly carry "S02" / "2nd" rather than the English rendering,
/// so substring-matching them produces zero hits and falls back to a
/// full-pack download anyway. Better to skip the selective path.
fn is_generic_season_subtitle(lower: &str) -> bool {
    matches!(
        lower,
        "first season"
            | "second season"
            | "third season"
            | "fourth season"
            | "fifth season"
            | "sixth season"
            | "seventh season"
            | "eighth season"
            | "ninth season"
            | "tenth season"
            | "1st season"
            | "2nd season"
            | "3rd season"
            | "4th season"
            | "5th season"
            | "6th season"
            | "7th season"
            | "8th season"
            | "9th season"
            | "10th season"
    ) || lower.starts_with("part ")
        || lower.starts_with("chapter ")
}

/// True when the target has a discriminator that [`pick_wanted_file_indices`]
/// can use to narrow a megapack — part number or own subtitle. Gate at
/// the call sites so the expensive metadata-wait path is only entered
/// when it has a chance of actually narrowing the file list.
///
/// Franchise roots without their own subtitle (JoJo S1 = "JoJo's
/// Bizarre Adventure") deliberately return `false` here — they're
/// handled by the higher-level multi-series pack auto-expansion path,
/// not by filename-based negative matching, which produces silent
/// wrong-selections when AniList relations only list direct siblings.
pub fn has_selective_discriminator(detail: &AnimeDetail) -> bool {
    extract_part_number(detail).is_some() || extract_season_subtitle(detail).is_some()
}

/// A sibling anime entry detected in the filename list of a megapack
/// torrent — i.e. a related series (sequel, prequel, side story, …)
/// of the parent target whose own files are also present in the pack.
///
/// Produced by [`detect_sibling_entries_in_pack`] and consumed by the
/// library auto-expand path in `handlers::library`, which upserts each
/// sibling into the tracked series table and records per-file routing
/// so post-processing can move each file into the correct media
/// folder.
///
/// All title / cover / format fields come straight from the parent
/// detail's `relations` card so `series::upsert` has enough to
/// populate a complete row without a second metadata fetch.
#[derive(Debug, Clone)]
pub struct SiblingMatch {
    pub anilist_id: i64,
    pub mal_id: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    /// The subtitle that produced the match (e.g. "Stardust
    /// Crusaders"). Logged at grab time so the operator can see *why*
    /// a sibling was picked up.
    pub matched_subtitle: String,
    /// Indices into the torrent's file list of files that belong to
    /// this sibling. Each file index is unique across the full return
    /// value — [`detect_sibling_entries_in_pack`] resolves overlaps by
    /// longest-subtitle-wins.
    pub file_indices: Vec<usize>,
}

/// Relation types we'll consider in-pack candidates when scanning an
/// AniList relation graph for siblings. Excludes:
///
/// - **Source material** (`ADAPTATION`, `SOURCE`, `COMPILATION`,
///   `CONTAINS`) — these point at manga / LN / book entries that
///   will never appear in an anime torrent.
/// - **Off-series tie-ins** (`CHARACTER`, `OTHER`) — `CHARACTER`
///   links to shared-universe spinoffs that share no animation DNA
///   with the parent (e.g. a crossover cameo), and `OTHER` is
///   AniList's dumping ground for unusual relations (promotional
///   videos, live-action adaptations, disambiguation links, etc.).
///   Both are noisy enough that including them mostly pads the
///   candidate list with entries that never match.
///
/// Everything else — `SEQUEL`, `PREQUEL`, `SIDE_STORY`, `PARENT`,
/// `ALTERNATIVE`, `SPIN_OFF`, `SUMMARY` — passes through because the
/// downstream subtitle-match + episode-count cap are already doing
/// the real false-positive filtering. This gate is a performance
/// filter that avoids normalizing obviously-wrong candidates against
/// every filename.
fn is_pack_candidate_relation(relation_type: &str) -> bool {
    !matches!(
        relation_type,
        "ADAPTATION" | "SOURCE" | "COMPILATION" | "CONTAINS" | "CHARACTER" | "OTHER"
    )
}

/// Detect sibling anime entries (sequel / prequel / side story /
/// etc. of the parent) whose own episodes are present in a megapack
/// release's file list.
///
/// **Provenance gate:** returns an empty `Vec` when
/// `parent_detail.id <= 0`. Negative IDs are the Jikan fallback
/// sentinel (`-mal_id`) and non-positive IDs are not AniList entries.
/// Jikan's relations scrape reflects MAL's graph, and MAL splits
/// sagas that AniList merges (Stone Ocean is 3 MAL entries vs 1 AL
/// entry), so auto-adding MAL siblings against an AL-sourced parent
/// would duplicate library rows. When AL is down, the grab still
/// proceeds — it just skips sibling expansion — and the background
/// 12h metadata refresh will retroactively run detection the next
/// time AL returns the relation list.
///
/// **Overlap resolution:** when a filename matches more than one
/// sibling subtitle (e.g. "Stardust" ⊂ "Stardust Crusaders", or a
/// freak collision between two unrelated sibling titles), the longer
/// normalized subtitle wins. Each file index appears in exactly one
/// `SiblingMatch::file_indices` across the return value.
///
/// **Episode-count cap:** each sibling's match set is rejected if it
/// overshoots the sibling's own AniList `episodes` count by ×1.5 + 2.
/// Matches with `episodes: None` bypass the cap (airing series, which
/// the downstream grab path handles anyway).
///
/// Callers get a best-effort list — siblings whose title has no
/// trailing subtitle (e.g. a franchise root like "Naruto Shippuden")
/// are silently skipped, matching the conservative behavior of
/// `pick_wanted_file_indices`.
pub fn detect_sibling_entries_in_pack(
    filenames: &[String],
    parent_detail: &AnimeDetail,
) -> Vec<SiblingMatch> {
    if parent_detail.id <= 0 {
        return Vec::new();
    }

    // Candidates: one entry per relation that produced a usable
    // subtitle. Stored by index into `parent_detail.relations` to
    // avoid borrowing complications during the materialize pass.
    let mut candidates: Vec<(usize, String, String)> = Vec::new(); // (rel_idx, raw subtitle, normalized needle)
    for (rel_idx, rel) in parent_detail.relations.iter().enumerate() {
        if !rel.media_type.eq_ignore_ascii_case("ANIME") {
            continue;
        }
        if !is_pack_candidate_relation(&rel.relation_type) {
            continue;
        }
        let sibling_title = if !rel.title_english.is_empty() {
            rel.title_english.as_str()
        } else if !rel.title_romaji.is_empty() {
            rel.title_romaji.as_str()
        } else {
            continue;
        };
        let Some(subtitle) = trailing_subtitle_of(sibling_title) else {
            continue;
        };
        let needle = normalize_subtitle(&subtitle);
        if needle.is_empty() {
            continue;
        }
        candidates.push((rel_idx, subtitle, needle));
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    // First pass: for each media file, pick the candidate with the
    // LONGEST normalized needle that substring-matches the filename.
    // Longest-wins handles the Stardust ⊂ Stardust Crusaders case
    // without ever double-counting a file.
    let mut winner_by_file: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new(); // file_idx → (candidate_idx, needle_len)
    for (file_idx, name) in filenames.iter().enumerate() {
        if !is_media_filename(name) {
            continue;
        }
        let normalized = normalize_subtitle(name);
        let mut best: Option<(usize, usize)> = None; // (candidate_idx, needle_len)
        for (cand_idx, (_, _, needle)) in candidates.iter().enumerate() {
            if !normalized.contains(needle) {
                continue;
            }
            match best {
                Some((_, cur_len)) if cur_len >= needle.len() => {}
                _ => best = Some((cand_idx, needle.len())),
            }
        }
        if let Some(pick) = best {
            winner_by_file.insert(file_idx, pick);
        }
    }

    // Bucket files by winning candidate.
    let mut per_candidate: Vec<Vec<usize>> = vec![Vec::new(); candidates.len()];
    for (file_idx, (cand_idx, _)) in winner_by_file {
        per_candidate[cand_idx].push(file_idx);
    }
    for list in per_candidate.iter_mut() {
        list.sort_unstable();
    }

    // Materialize results. Drop candidates with no files and enforce
    // the per-sibling episode-count sanity cap.
    let mut out: Vec<SiblingMatch> = Vec::new();
    for (cand_idx, (rel_idx, subtitle, _)) in candidates.into_iter().enumerate() {
        let file_indices = std::mem::take(&mut per_candidate[cand_idx]);
        if file_indices.is_empty() {
            continue;
        }
        let rel: &RelatedEntry = &parent_detail.relations[rel_idx];
        if !within_episode_slack(file_indices.len(), rel.episodes.unwrap_or(0)) {
            continue;
        }
        out.push(SiblingMatch {
            anilist_id: rel.id,
            mal_id: rel.id_mal,
            title_romaji: rel.title_romaji.clone(),
            title_english: rel.title_english.clone(),
            title_native: rel.title_native.clone(),
            cover_url: rel.cover_url.clone(),
            format: rel.format.clone(),
            status: rel.status.clone(),
            episodes: rel.episodes,
            season_year: rel.season_year,
            matched_subtitle: subtitle,
            file_indices,
        });
    }

    out
}

/// Lowercase ASCII-alphanumeric chars and collapse non-alphanumeric
/// runs to single spaces. Used to make subtitle-vs-filename comparisons
/// robust to punctuation differences like "JoJo's" vs "JoJos",
/// "Stardust-Crusaders" vs "Stardust Crusaders", or brackets.
fn normalize_subtitle(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim().to_string()
}

/// Is this filename likely a media file that `parse_release_numbers`
/// should even be run against? Used by [`pick_wanted_file_indices`] to
/// stop non-media files (NFOs, subtitles, samples) from inflating the
/// media count or being accidentally kept/skipped.
pub(crate) fn is_media_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.rsplit('.').next(),
        Some("mkv")
            | Some("mp4")
            | Some("avi")
            | Some("m2ts")
            | Some("ts")
            | Some("mov")
            | Some("wmv")
    )
}

/// Check if a release's season conflicts with the expected season.
/// Returns true if there is a definite mismatch.
fn season_mismatch(release_title: &str, expected_season: i32) -> bool {
    let release_season = parse_release_season(release_title);
    if release_season == 0 {
        // No season indicator in release — allow it (could be absolute numbering)
        return false;
    }
    let effective_expected = if expected_season > 0 { expected_season } else { 1 };
    release_season != effective_expected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::anilist::AnimeDetail;

    fn detail_with(format: &str, episodes: Option<i32>) -> AnimeDetail {
        AnimeDetail {
            id: 1,
            id_mal: None,
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: format.to_string(),
            status: String::new(),
            status_display: String::new(),
            episodes,
            duration: None,
            season: String::new(),
            season_year: None,
            end_year: None,
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

    // SearchTarget::for_episode collapses to Single for single-entry media
    // so per-episode handlers don't pass Episode(n) for shows that don't
    // have episode numbers in their release filenames.

    #[test]
    fn for_episode_collapses_movie_to_single() {
        let d = detail_with("MOVIE", Some(1));
        assert!(matches!(SearchTarget::for_episode(&d, 1), SearchTarget::Single));
    }

    #[test]
    fn for_episode_collapses_special_to_single() {
        let d = detail_with("SPECIAL", Some(1));
        assert!(matches!(SearchTarget::for_episode(&d, 1), SearchTarget::Single));
    }

    #[test]
    fn for_episode_collapses_ova_to_single() {
        let d = detail_with("OVA", Some(1));
        assert!(matches!(SearchTarget::for_episode(&d, 1), SearchTarget::Single));
    }

    #[test]
    fn for_episode_keeps_episode_for_single_episode_tv() {
        // TV format stays as Episode(n) regardless of episode count — the
        // collapse rule is format-only. A TV release titled "Show - 01" still
        // carries an episode number that Episode(1) can match against.
        let d = detail_with("TV", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Episode(1)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_tv() {
        let d = detail_with("TV", Some(12));
        assert!(matches!(
            SearchTarget::for_episode(&d, 7),
            SearchTarget::Episode(7)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_when_episode_count_unknown() {
        // AniList reports episodes=None for currently-airing shows — the
        // fallback should still be Episode(n) because that's the correct
        // target for an airing weekly release.
        let d = detail_with("TV", None);
        assert!(matches!(
            SearchTarget::for_episode(&d, 3),
            SearchTarget::Episode(3)
        ));
    }

    #[test]
    fn for_episode_collapses_movie_even_when_episode_count_is_none() {
        // MOVIE always collapses regardless of AniList's episode count — a
        // film is single-entry even if AniList has weird/missing data.
        let d = detail_with("MOVIE", None);
        assert!(matches!(SearchTarget::for_episode(&d, 1), SearchTarget::Single));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_ova() {
        // Multi-episode OVA series (e.g., long-running OVA franchises with
        // 10+ entries) carry episode numbers in their release filenames,
        // so per-episode search must NOT collapse them to Single — that
        // would return a release for any episode or a full batch when the
        // user specifically asked for episode N.
        let d = detail_with("OVA", Some(10));
        assert!(matches!(
            SearchTarget::for_episode(&d, 5),
            SearchTarget::Episode(5)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_ona() {
        let d = detail_with("ONA", Some(24));
        assert!(matches!(
            SearchTarget::for_episode(&d, 12),
            SearchTarget::Episode(12)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_special() {
        let d = detail_with("SPECIAL", Some(4));
        assert!(matches!(
            SearchTarget::for_episode(&d, 2),
            SearchTarget::Episode(2)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_ova_with_unknown_count() {
        // Ambiguous: we don't know whether it's a 1-episode OVA or a
        // 12-episode OVA. Safe default is Episode(n) — the failure mode
        // is "no results" rather than "grabbed the wrong release".
        let d = detail_with("OVA", None);
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Episode(1)
        ));
    }

    // split_title_segments uses a 2-token minimum to reject segments that
    // are too generic to safely become matching aliases. These tests cover
    // the rule in isolation with abstract inputs so the behavior is
    // described, not tied to any particular show.

    #[test]
    fn split_segments_keeps_three_token_subtitle() {
        let segments = split_title_segments("Main Title: Sub One Two Three");
        assert!(
            segments.iter().any(|s| s == "Sub One Two Three"),
            "multi-word subtitle should be kept as a segment, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_keeps_two_token_subtitle() {
        // Two whitespace-separated tokens is the minimum.
        let segments = split_title_segments("Main Title: Alpha Beta");
        assert!(
            segments.iter().any(|s| s == "Alpha Beta"),
            "two-token subtitle should be kept, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_rejects_single_word_subtitle() {
        let segments = split_title_segments("Main Title: Singleword");
        assert!(
            !segments.iter().any(|s| s == "Singleword"),
            "single-word subtitle should be rejected, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_rejects_hyphenated_single_word() {
        // Hyphens are not whitespace, so "Hyphen-Word" is still one token
        // under the rule — important because hyphenated English phrases
        // like "Iron-Blooded" are common enough to substring-match many
        // unrelated titles.
        let segments = split_title_segments("Main Title: Hyphen-Word");
        assert!(
            !segments.iter().any(|s| s == "Hyphen-Word"),
            "hyphenated single-word segment should be rejected, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_keeps_multi_word_main_portion() {
        // Even when the subtitle is rejected, the leading multi-word
        // portion of a compound title remains usable.
        let segments = split_title_segments("Main Title Two: Singleword");
        assert!(
            segments.iter().any(|s| s == "Main Title Two"),
            "multi-word leading portion should be kept, got {:?}",
            segments
        );
    }

    #[test]
    fn matches_target_rejects_release_whose_only_overlap_is_a_rejected_segment() {
        // End-to-end regression: a release whose token overlap with the
        // primary alias is below the 0.6 threshold must not slip through
        // just because some single-word substring of a synonym happens to
        // appear in the release filename. With the 2-token rule in place,
        // that single-word substring is never produced as an alias, so
        // substring-match can't succeed.
        let aliases = vec![
            "Main Title: Subtitle One".to_string(),
            "Main Title: Subtitle Two".to_string(),
        ];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let unrelated_release =
            "[Group] Totally Different Show - Subtitle One-Word Thing - 01 [1080p].mkv";
        // The release shares only the word "Subtitle" with the primary
        // alias tokens {main, title, subtitle, one} / {main, title,
        // subtitle, two}. Overlap ratio for either alias is 1/4 = 0.25,
        // well below 0.6. No segment derived from the primary aliases
        // survives the 2-token rule to substring-match "Subtitle" in
        // isolation, so the match must fail.
        assert!(
            !matches_target(
                unrelated_release,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(1),
                0,
                false,
            ),
            "unrelated release should not match via token overlap alone"
        );
    }

    #[test]
    fn matches_target_accepts_release_with_full_primary_alias_substring() {
        let aliases = vec!["Main Title Subtitle One".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let good_release = "[Group] Main Title Subtitle One [BD 1080p].mkv";
        assert!(matches_target(
            good_release,
            &aliases,
            &no_siblings,
            &SearchTarget::Single,
            0,
            false,
        ));
    }

    #[test]
    fn matches_target_rejects_sibling_arc_release() {
        // Regression: auto-searching JJK S1 E6 used to grab
        // `[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06`
        // because the sibling arc title has no explicit "S02"/"Season 2"
        // marker for `season_mismatch` to catch, but "Jujutsu Kaisen" is
        // a substring of the release. The sibling check resolves this:
        // the sibling alias "Jujutsu Kaisen: Shimetsu Kaiyuu" has 4
        // overlapping tokens with the release vs the target's 2, so the
        // sibling wins and the release is rejected.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let siblings = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 [1080p CR WEBRip HEVC AAC].mkv";
        assert!(
            !matches_target(
                release,
                &own,
                &precompute,
                &SearchTarget::Episode(6),
                1,
                false,
            ),
            "sibling arc release must not match the base-franchise target"
        );
    }

    #[test]
    fn matches_target_keeps_base_franchise_release_despite_siblings() {
        // Symmetric: with the same sibling list, a plain JJK S1 release
        // should still match the target. The sibling overlaps on only
        // 2 tokens ({jujutsu, kaisen}) — the same as the target's own
        // overlap — so the sibling check is a no-op.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let siblings = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen - 06 [1080p].mkv";
        assert!(matches_target(
            release,
            &own,
            &precompute,
            &SearchTarget::Episode(6),
            1,
            false,
        ));
    }

    #[test]
    fn matches_target_keeps_target_arc_release_against_unrelated_sibling() {
        // A JJK S2 Shibuya Incident target should still accept its own
        // arc release even when the sibling list includes another arc.
        let own = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let siblings = vec![
            "Jujutsu Kaisen".to_string(),
            "Jujutsu Kaisen: Kaigyoku Gyokusetsu".to_string(),
        ];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 [1080p].mkv";
        assert!(matches_target(
            release,
            &own,
            &precompute,
            &SearchTarget::Episode(6),
            0,
            false,
        ));
    }

    #[test]
    fn format_scoring_detail_matches_plan_docs_example() {
        // Shape documented in plan §6.3 — the scoring log entry should
        // read like this exact format so users grepping the logs can
        // rely on a stable column layout. The CF names below ("10bit
        // x265", "FLAC audio", "Preferred Groups: MTBB") are reproduced
        // verbatim from the plan doc example; they're opaque label
        // strings passed through by the formatter, not claims about
        // any release group's actual naming scheme.
        let breakdown = vec![
            ("10bit x265".to_string(), 200),
            ("FLAC audio".to_string(), 120),
            ("Preferred Groups: MTBB".to_string(), 100),
        ];
        let s = format_scoring_detail(85, 420, &breakdown, 0, 505, false);
        assert_eq!(
            s,
            "base=85, cf=+420 [10bit x265 +200, FLAC audio +120, Preferred Groups: MTBB +100], seadex=0, final=505"
        );
    }

    #[test]
    fn format_scoring_detail_empty_breakdown_drops_bracket_section() {
        // No CFs matched → the bracket section is noise. Just show the
        // scalar cf= total.
        let s = format_scoring_detail(50, 0, &[], 0, 50, false);
        assert_eq!(s, "base=50, cf=+0, seadex=0, final=50");
    }

    #[test]
    fn format_scoring_detail_negative_cf_has_sign_and_marks_drop() {
        let breakdown = vec![("Casual group penalty".to_string(), -1000)];
        let s = format_scoring_detail(20, -1000, &breakdown, 0, -980, true);
        assert_eq!(
            s,
            "base=20, cf=-1000 [Casual group penalty -1000], seadex=0, final=-980 DROPPED(below minimum_score floor)"
        );
    }

    // ── extract_part_number / pick_wanted_file_indices ────────────────────
    //
    // These cover the selective-file download path that lets Ryokan grab
    // just one entry out of a multi-part megapack (Kizumonogatari I/II/III
    // in a single smol release, Rebuild of Evangelion 1.0/2.0/3.0, etc.).

    fn detail_with_titles(english: &str, romaji: &str) -> AnimeDetail {
        AnimeDetail {
            id: 1,
            id_mal: None,
            title_romaji: romaji.to_string(),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "MOVIE".to_string(),
            status: String::new(),
            status_display: String::new(),
            episodes: Some(1),
            duration: None,
            season: String::new(),
            season_year: None,
            end_year: None,
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

    #[test]
    fn extract_part_number_parses_roman_ii() {
        let d = detail_with_titles("Kizumonogatari II: Nekketsu-hen", "Kizumonogatari II: Nekketsu-hen");
        assert_eq!(extract_part_number(&d), Some(2));
    }

    #[test]
    fn extract_part_number_parses_roman_iii() {
        let d = detail_with_titles("Kizumonogatari III: Reiketsu-hen", "Kizumonogatari III: Reiketsu-hen");
        assert_eq!(extract_part_number(&d), Some(3));
    }

    #[test]
    fn extract_part_number_parses_explicit_part_n() {
        let d = detail_with_titles("Some Show Part 2", "Some Show Part 2");
        assert_eq!(extract_part_number(&d), Some(2));
    }

    #[test]
    fn extract_part_number_returns_none_for_single_entry() {
        // A standalone film with no part marker at all.
        let d = detail_with_titles("A Silent Voice", "Koe no Katachi");
        assert_eq!(extract_part_number(&d), None);
    }

    #[test]
    fn extract_part_number_drops_bare_roman_i_on_kizu_first_entry() {
        // Kizumonogatari I no longer resolves to Some(1) — see the
        // RE_ROMAN_PART docstring. Dropping bare single-letter Romans
        // trades selective narrowing for entry 1 of a trilogy against
        // not false-positiving any title containing an English pronoun
        // "I". Users who want Kizu I specifically still get the whole
        // smol pack (no selective narrowing), which is the acceptable
        // fallback for this edge case.
        let d = detail_with_titles("Kizumonogatari I: Tekketsu-hen", "Kizumonogatari I: Tekketsu-hen");
        assert_eq!(extract_part_number(&d), None);
    }

    #[test]
    fn extract_part_number_rejects_bare_roman_on_english_pronoun() {
        // "I Want to Eat Your Pancreas" must NOT resolve to Some(1).
        // Otherwise, if the user ever grabbed a Monogatari-style
        // megapack with this detail as the target, pick_by_part_number
        // would narrow to "files containing episode 1" for an
        // unrelated film. The same concern motivates dropping bare V
        // ("V for Vendetta") and bare X ("X/1999").
        let d = detail_with_titles(
            "I Want to Eat Your Pancreas",
            "Kimi no Suizou wo Tabetai",
        );
        assert_eq!(extract_part_number(&d), None);
    }

    #[test]
    fn pick_wanted_file_indices_narrows_smol_monogatari_pack() {
        // Simulates the smol Monogatari megapack. The filenames carry
        // standard S09EXX numbering that `parse_release_numbers` picks
        // up, and the target's part number (from the AniList title's
        // Roman numeral) selects the right file.
        let files = vec![
            "[smol] Monogatari - S09E01 - Kizumonogatari Tekketsu-hen.mkv".to_string(),
            "[smol] Monogatari - S09E02 - Kizumonogatari Nekketsu-hen.mkv".to_string(),
            "[smol] Monogatari - S09E03 - Kizumonogatari Reiketsu-hen.mkv".to_string(),
        ];
        let d = detail_with_titles("Kizumonogatari II: Nekketsu-hen", "Kizumonogatari II");
        let picked = pick_wanted_file_indices(&files, &d).expect("should narrow");
        assert_eq!(picked, vec![1]);
    }

    #[test]
    fn pick_wanted_file_indices_returns_none_when_target_has_no_part() {
        // Standalone single-file release — nothing to narrow against.
        let files = vec!["[Group] Some Film (BD 1080p).mkv".to_string()];
        let d = detail_with_titles("A Silent Voice", "Koe no Katachi");
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn pick_wanted_file_indices_returns_none_when_no_match() {
        // Target is part 2 but the pack doesn't contain an E02 file.
        // Safer to keep everything than to skip every file.
        let files = vec![
            "[Group] Show - 01.mkv".to_string(),
            "[Group] Show - 03.mkv".to_string(),
        ];
        let d = detail_with_titles("Show II", "Show II");
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn pick_wanted_file_indices_ignores_non_media_files() {
        let files = vec![
            "[Group] Pack - 01.mkv".to_string(),
            "[Group] Pack - 02.mkv".to_string(),
            "[Group] Pack - 02.nfo".to_string(),
            "[Group] Pack - 02.txt".to_string(),
        ];
        let d = detail_with_titles("Show II", "Show II");
        let picked = pick_wanted_file_indices(&files, &d).expect("should narrow");
        // Only the E02 .mkv survives — the .nfo and .txt with "02"
        // in their names are discarded before matching.
        assert_eq!(picked, vec![1]);
    }

    #[test]
    fn pick_wanted_file_indices_returns_none_when_every_media_file_matches() {
        // Not actually a megapack — every media file carries the target
        // number (e.g. a single-movie torrent with sample + main file).
        // Don't mess with priorities.
        let files = vec![
            "[Group] Show II (BD 1080p).mkv".to_string(),
            "[Group] Show II (BD 1080p) - sample.mkv".to_string(),
        ];
        let d = detail_with_titles("Show II", "Show II");
        // Neither file carries an episode number, so parse_release_numbers
        // returns an empty set — no matches, None returned.
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn format_scoring_detail_surfaces_seadex_bonus() {
        // SeaDex bonus is the only non-CF overlay; make sure it shows
        // up in the final line so the log reader can tell "SeaDex hit"
        // apart from "CF scoring pushed this above everything else."
        let breakdown = vec![("x265".to_string(), 300)];
        let s = format_scoring_detail(60, 300, &breakdown, 10000, 10360, false);
        assert_eq!(
            s,
            "base=60, cf=+300 [x265 +300], seadex=10000, final=10360"
        );
    }

    // ── extract_season_subtitle / positive subtitle match ────────────────
    //
    // Covers the second narrowing strategy in `pick_wanted_file_indices` —
    // positive subtitle match for titles with a distinguishing suffix.
    // Franchise roots without their own subtitle (JoJo S1) are NOT
    // narrowed here; they flow through to the multi-series pack
    // auto-expansion path instead.

    #[test]
    fn extract_season_subtitle_pulls_named_season() {
        // Positive case: the English title ends in `: Stardust Crusaders`,
        // which is a distinctive multi-token phrase.
        let d = detail_with_titles(
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "JoJo no Kimyou na Bouken: Stardust Crusaders",
        );
        assert_eq!(extract_season_subtitle(&d).as_deref(), Some("Stardust Crusaders"));
    }

    #[test]
    fn extract_season_subtitle_pulls_from_dash_delimited_title() {
        // En-dash / hyphen-space delimiter also produces a subtitle.
        let d = detail_with_titles("Fate/stay night - Unlimited Blade Works", "");
        assert_eq!(extract_season_subtitle(&d).as_deref(), Some("Unlimited Blade Works"));
    }

    #[test]
    fn extract_season_subtitle_rejects_single_token_suffix() {
        // "Brotherhood" alone is too generic — it could substring-match
        // an unrelated filename fragment. The 2-token minimum blocks it.
        let d = detail_with_titles("Fullmetal Alchemist: Brotherhood", "");
        assert_eq!(extract_season_subtitle(&d), None);
    }

    #[test]
    fn extract_season_subtitle_rejects_generic_ordinal_season() {
        // "Second Season" is a pure ordinal marker — release filenames
        // carry "S02" / "2nd" rather than the English spelling, so
        // matching on this would yield zero hits and fall back to the
        // full pack anyway. Skip it upfront.
        let d = detail_with_titles("Monogatari Series: Second Season", "");
        assert_eq!(extract_season_subtitle(&d), None);
    }

    #[test]
    fn extract_season_subtitle_returns_none_without_delimiter() {
        // Franchise root with no subtitle — handled by the
        // multi-series pack auto-expansion path, not here.
        let d = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        assert_eq!(extract_season_subtitle(&d), None);
    }

    #[test]
    fn extract_season_subtitle_preserves_re_zero_style_colon() {
        // "Re:Zero kara Hajimeru Isekai Seikatsu" — the `:` has no
        // following space, so it should NOT be split. The trailing
        // segment rule returns the whole title, which equals the
        // original → None.
        let d = detail_with_titles("Re:Zero kara Hajimeru Isekai Seikatsu", "");
        assert_eq!(extract_season_subtitle(&d), None);
    }

    #[test]
    fn pick_wanted_file_indices_narrows_by_subtitle_positive_match() {
        // Simulates a JoJo franchise megapack. The target carries its
        // own distinguishing subtitle ("Stardust Crusaders") that
        // appears in the S2 filenames but not in the S1 / S3 / S4 ones.
        let files = vec![
            "[Group] JoJo's Bizarre Adventure - 01.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - 26.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Stardust Crusaders - 01.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Stardust Crusaders - 48.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Diamond is Unbreakable - 01.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Golden Wind - 01.mkv".to_string(),
        ];
        let mut d = detail_with_titles(
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "JoJo no Kimyou na Bouken: Stardust Crusaders",
        );
        // JoJo S2 is 48 episodes — sanity cap (×1.5 + 2 = 74) passes.
        d.episodes = Some(48);
        let picked = pick_wanted_file_indices(&files, &d).expect("should narrow");
        assert_eq!(picked, vec![2, 3]);
    }

    #[test]
    fn pick_wanted_file_indices_returns_none_for_subtitleless_franchise_root() {
        // JoJo S1 — no subtitle, no part number. The selective path is
        // intentionally not used here. The grab handler's multi-series
        // auto-expansion (Phase 2) is what handles this case.
        let files = vec![
            "[Group] JoJo's Bizarre Adventure - 01.mkv".to_string(),
            "[Group] JoJo's Bizarre Adventure - Stardust Crusaders - 01.mkv".to_string(),
        ];
        let d = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn pick_wanted_file_indices_rejects_overshoot_via_episode_cap() {
        // Contrived pathological case: the target's subtitle is a
        // prefix of every file in a much larger pack. Episode count
        // says 12 but the match set is 50 — the cap fires and we
        // return None rather than producing a wildly-wrong narrowing.
        let files: Vec<String> = (1..=50)
            .map(|i| format!("[Group] Show - Alpha Beta Ep{:02}.mkv", i))
            .collect();
        let mut d = detail_with_titles("Show: Alpha Beta", "");
        d.episodes = Some(12);
        assert_eq!(pick_wanted_file_indices(&files, &d), None);
    }

    #[test]
    fn has_selective_discriminator_true_for_part_number_title() {
        let d = detail_with_titles("Kizumonogatari II: Nekketsu-hen", "");
        assert!(has_selective_discriminator(&d));
    }

    #[test]
    fn has_selective_discriminator_true_for_subtitle_title() {
        let d = detail_with_titles("JoJo's Bizarre Adventure: Stardust Crusaders", "");
        assert!(has_selective_discriminator(&d));
    }

    #[test]
    fn has_selective_discriminator_false_for_standalone_single() {
        // No part number, no subtitle — selective path skipped.
        let d = detail_with_titles("A Silent Voice", "Koe no Katachi");
        assert!(!has_selective_discriminator(&d));
    }

    #[test]
    fn has_selective_discriminator_false_for_franchise_root() {
        // Franchise root without its own subtitle — selective path
        // skipped on purpose. Multi-series auto-expansion handles it.
        let d = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        assert!(!has_selective_discriminator(&d));
    }

    // ── detect_sibling_entries_in_pack ──────────────────────────────

    fn related(
        id: i64,
        english: &str,
        romaji: &str,
        relation_type: &str,
        episodes: Option<i32>,
    ) -> RelatedEntry {
        RelatedEntry {
            id,
            id_mal: None,
            title_romaji: romaji.to_string(),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes,
            relation_type: relation_type.to_string(),
            season_year: None,
            media_type: "ANIME".to_string(),
        }
    }

    #[test]
    fn detect_siblings_finds_named_seasons_in_jojo_pack() {
        // Parent: JoJo S1 (franchise root, no subtitle of its own).
        // Pack contains files for S1 (no subtitle), S3 Stardust
        // Crusaders, and S4 Diamond is Unbreakable. Detection should
        // return two sibling matches (Stardust + Diamond) with only
        // their own files; S1 files stay unclaimed.
        let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        parent.id = 14719; // AL id
        parent.episodes = Some(26);
        parent.relations = vec![
            related(
                20800,
                "JoJo's Bizarre Adventure: Stardust Crusaders",
                "JoJo no Kimyou na Bouken: Stardust Crusaders",
                "SEQUEL",
                Some(24),
            ),
            related(
                31292,
                "JoJo's Bizarre Adventure: Diamond is Unbreakable",
                "JoJo no Kimyou na Bouken: Diamond wa Kudakenai",
                "SEQUEL",
                Some(39),
            ),
        ];

        let files: Vec<String> = vec![
            // S1 files (unclaimed)
            "[Group] JoJo no Kimyou na Bouken - 01.mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - 02.mkv".to_string(),
            // Stardust Crusaders (24 eps, we include just 3 for brevity)
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01.mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 02.mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 03.mkv".to_string(),
            // Diamond is Unbreakable
            "[Group] JoJo no Kimyou na Bouken - Diamond is Unbreakable - 01.mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - Diamond is Unbreakable - 02.mkv".to_string(),
        ];

        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 2, "expected Stardust + Diamond matches");

        let stardust = siblings
            .iter()
            .find(|s| s.anilist_id == 20800)
            .expect("stardust sibling present");
        assert_eq!(stardust.file_indices, vec![2, 3, 4]);
        assert!(
            stardust.matched_subtitle.to_lowercase().contains("stardust"),
            "matched_subtitle should reference Stardust, got {:?}",
            stardust.matched_subtitle
        );

        let diamond = siblings
            .iter()
            .find(|s| s.anilist_id == 31292)
            .expect("diamond sibling present");
        assert_eq!(diamond.file_indices, vec![5, 6]);
    }

    #[test]
    fn detect_siblings_returns_empty_for_jikan_sourced_detail() {
        // Provenance gate: Jikan-sourced details have id < 0. Even
        // if relations look plausible, we must not run sibling
        // detection against them — MAL splits sagas AL merges, which
        // would duplicate library rows.
        let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        parent.id = -1; // Jikan sentinel
        parent.relations = vec![related(
            -20800,
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "",
            "SEQUEL",
            Some(24),
        )];
        let files: Vec<String> = vec![
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01.mkv".to_string(),
        ];
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_resolves_overlap_by_longest_subtitle() {
        // Two siblings whose subtitles form a prefix relationship. A
        // filename containing the longer subtitle matches both
        // normalized needles, but the longer one must win — otherwise
        // we'd double-count the file.
        let mut parent = detail_with_titles("Franchise", "Franchise");
        parent.id = 100;
        parent.relations = vec![
            related(201, "Franchise: Alpha", "", "SEQUEL", Some(12)),
            related(202, "Franchise: Alpha Prime", "", "SEQUEL", Some(12)),
        ];
        let files: Vec<String> = vec![
            "[Group] Franchise - Alpha Prime - 01.mkv".to_string(),
            "[Group] Franchise - Alpha Prime - 02.mkv".to_string(),
        ];
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0].anilist_id, 202);
        assert_eq!(siblings[0].file_indices, vec![0, 1]);
    }

    #[test]
    fn detect_siblings_skips_relations_without_own_subtitle() {
        // "Naruto Shippuden" has no trailing delimiter so
        // trailing_subtitle_of returns None and the sibling gets
        // silently dropped. This is intentional — without a
        // subtitle we can't safely narrow a filename list, so
        // conservative over-skipping is the right call.
        let mut parent = detail_with_titles("Naruto", "Naruto");
        parent.id = 20;
        parent.relations = vec![related(1735, "Naruto Shippuden", "", "SEQUEL", Some(500))];
        let files: Vec<String> = vec!["[Group] Naruto Shippuden - 01.mkv".to_string()];
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_rejects_episode_count_overshoot() {
        // A sibling with episodes=12 whose subtitle accidentally
        // matches 50 files in the pack. The episode-count cap
        // (×1.5 + 2 = 20) fires and drops the sibling entirely
        // rather than emitting a wildly-wrong routing.
        let mut parent = detail_with_titles("Franchise", "Franchise");
        parent.id = 100;
        parent.relations = vec![related(201, "Franchise: Alpha Beta", "", "SEQUEL", Some(12))];
        let files: Vec<String> = (1..=50)
            .map(|i| format!("[Group] Franchise - Alpha Beta - {:02}.mkv", i))
            .collect();
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_filters_out_source_material_relations() {
        // ADAPTATION / SOURCE / COMPILATION / CONTAINS relations
        // point at manga / LN / book entries that will never appear
        // in an anime torrent. Even if one happened to share a
        // substring with a filename, the relation-type gate must
        // drop it before we waste cycles on string matching.
        let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "");
        parent.id = 14719;
        parent.relations = vec![related(
            2,
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "",
            "SOURCE",
            Some(1),
        )];
        let files: Vec<String> = vec![
            "[Group] JoJo - Stardust Crusaders - 01.mkv".to_string(),
        ];
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_ignores_non_anime_media_types() {
        // AL returns the parent manga via a relation edge with
        // media_type="MANGA". Never an anime torrent candidate.
        let mut parent = detail_with_titles("Show", "");
        parent.id = 10;
        let mut manga_rel = related(
            5,
            "Show: Spinoff Arc",
            "",
            "SIDE_STORY",
            Some(10),
        );
        manga_rel.media_type = "MANGA".to_string();
        parent.relations = vec![manga_rel];
        let files: Vec<String> = vec!["[Group] Show - Spinoff Arc - 01.mkv".to_string()];
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_passes_through_spin_off_and_summary_relations() {
        // Niche relation types (SPIN_OFF, SUMMARY, CHARACTER,
        // ALTERNATIVE) are included in the filter — the subtitle
        // match and episode-count cap do the downstream filtering.
        let mut parent = detail_with_titles("Show", "");
        parent.id = 10;
        parent.relations = vec![
            related(11, "Show: Recap Arc", "", "SUMMARY", Some(4)),
            related(12, "Show: Extra Chapter", "", "SPIN_OFF", Some(6)),
        ];
        let files: Vec<String> = vec![
            "[Group] Show - Recap Arc - 01.mkv".to_string(),
            "[Group] Show - Recap Arc - 02.mkv".to_string(),
            "[Group] Show - Extra Chapter - 01.mkv".to_string(),
        ];
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 2);
        let recap = siblings
            .iter()
            .find(|s| s.anilist_id == 11)
            .expect("recap sibling present");
        assert_eq!(recap.file_indices, vec![0, 1]);
        let extra = siblings
            .iter()
            .find(|s| s.anilist_id == 12)
            .expect("extra sibling present");
        assert_eq!(extra.file_indices, vec![2]);
    }

    #[test]
    fn detect_siblings_ignores_non_media_files_in_match_set() {
        // Subtitles, NFOs, samples etc. must not count toward the
        // episode cap or get routed. Only .mkv/.mp4/... files pass
        // through is_media_filename.
        let mut parent = detail_with_titles("Show", "");
        parent.id = 10;
        parent.relations = vec![related(11, "Show: Alpha Beta", "", "SEQUEL", Some(12))];
        let files: Vec<String> = vec![
            "[Group] Show - Alpha Beta - 01.mkv".to_string(),
            "[Group] Show - Alpha Beta - 01.srt".to_string(),
            "[Group] Show - Alpha Beta - readme.nfo".to_string(),
        ];
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1);
        // Only the .mkv file routes — the .srt and .nfo are filtered
        // out by is_media_filename before they can inflate the match
        // set.
        assert_eq!(siblings[0].file_indices, vec![0]);
    }
}
