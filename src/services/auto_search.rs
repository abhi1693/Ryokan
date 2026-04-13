use std::collections::HashSet;
use std::sync::LazyLock;

use chrono::{DateTime, Datelike};
use regex_lite::Regex;
use sqlx::SqlitePool;

use crate::models::config::Config;
use crate::services::source::{self, ClassificationResult, Resolution, Source};
use crate::services::{anilist::AnimeDetail, media, nyaa::{self, SearchOptions, SearchResult}, quality};

/// Convert a Unix timestamp to its calendar year.
///
/// Previously this was open-coded as `1970 + (ts / 31_536_000)`, which
/// assumes every year is exactly 365 days. That drifts by one day per
/// leap year and had already accumulated enough slippage that timestamps
/// near year boundaries were being bucketed into the wrong year — which
/// in turn fed the "finished series + 2 years" filter and caused a
/// handful of legit episodes to get rejected as "probably a sequel."
/// chrono handles leap years correctly and costs a few hundred
/// nanoseconds, which is nothing on this code path.
fn upload_year_of(ts: i64) -> Option<i32> {
    DateTime::from_timestamp(ts, 0).map(|dt| dt.year())
}

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

    let expected_season = infer_season_from_detail(detail);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    let ctx = InteractiveQueryCtx {
        aliases: &aliases,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        expected_season,
        is_finished,
        season_year: detail.season_year,
    };

    // Interactive search: allow batch results so user can see & pick them,
    // but filter by season and episode to avoid showing wrong-season results.
    run_queries_interactive(&queries, ctx, &mut seen, &mut candidates).await;

    // Try extended aliases if primary queries found nothing.
    if candidates.is_empty() {
        let extended = collect_extended_aliases(detail);
        if !extended.is_empty() {
            let ext_queries = build_queries_from_aliases(&extended, target);
            let all_aliases = [aliases.clone(), extended].concat();
            let ext_ctx = InteractiveQueryCtx { aliases: &all_aliases, ..ctx };
            run_queries_interactive(&ext_queries, ext_ctx, &mut seen, &mut candidates).await;
        }
    }

    if !preferred_groups.is_empty() {
        let group_queries = build_group_queries(detail, target, &preferred_groups);
        run_queries_interactive(&group_queries, ctx, &mut seen, &mut candidates).await;
    }

    for c in &mut candidates {
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
        c.score = rescore_for_auto_search(
            c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            detail.season_year,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
        );
    }

    candidates.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    candidates
}

pub async fn find_best_for_target(
    db: &SqlitePool,
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    allow_batch: bool,
    batch_episode_match: bool,
) -> Option<SearchResult> {
    collect_scored_for_target(db, detail, config, target, allow_batch, batch_episode_match)
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
) -> Option<SearchResult> {
    collect_scored_batches_for_target(db, detail, config, target)
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

    let expected_season = infer_season_from_detail(detail);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);

    let ctx = AutoQueryCtx {
        aliases: &aliases,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        allow_batch: true,
        expected_season,
        is_finished,
        season_year: detail.season_year,
        categories: &categories,
        batch_episode_match: false,
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

        c.score = rescore_for_auto_search(
            &c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            detail.season_year,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
        );
        scored.push(c);
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

    let expected_season = infer_season_from_detail(detail);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);

    let ctx = AutoQueryCtx {
        aliases: &aliases,
        preferred_groups: &preferred_groups,
        preferred_resolution: &preferred_res,
        target,
        allow_batch,
        expected_season,
        is_finished,
        season_year: detail.season_year,
        categories: &categories,
        batch_episode_match,
    };

    // Phase 1: standard queries (primary aliases + episode variants).
    run_queries(&queries, ctx, &mut seen, &mut candidates).await;

    // Phase 1.5: if no candidates, try extended aliases (synonyms + decomposed sub-phrases).
    if candidates.is_empty() {
        let extended = collect_extended_aliases(detail);
        if !extended.is_empty() {
            let ext_queries = build_queries_from_aliases(&extended, target);
            let all_aliases = [aliases.clone(), extended].concat();
            let ext_ctx = AutoQueryCtx { aliases: &all_aliases, ..ctx };
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

        c.score = rescore_for_auto_search(
            &c,
            &classification,
            config,
            &aliases,
            target,
            expected_season,
            is_finished,
            detail.season_year,
            finished_mode,
            preferred_source_enum,
            preferred_resolution_enum,
            cutoff_source_enum,
            cutoff_resolution_enum,
        );
        scored.push(c);
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
    preferred_groups: &'a [String],
    preferred_resolution: &'a str,
    target: &'a SearchTarget,
    allow_batch: bool,
    expected_season: i32,
    is_finished: bool,
    season_year: Option<i32>,
    categories: &'a [String],
    batch_episode_match: bool,
}

/// Same idea, but for the interactive-search helper which has a
/// smaller shared context and no category/batch override.
#[derive(Clone, Copy)]
struct InteractiveQueryCtx<'a> {
    aliases: &'a [String],
    preferred_groups: &'a [String],
    preferred_resolution: &'a str,
    target: &'a SearchTarget,
    expected_season: i32,
    is_finished: bool,
    season_year: Option<i32>,
}

/// Should this Nyaa result be dropped as a probable-sequel false positive?
///
/// For FINISHED series, any result uploaded 2+ years after the season aired
/// is much more likely to be for a later season than the one we're
/// searching (Nyaa listings aren't tagged with AniList IDs, so we have
/// nothing else to distinguish them). Single-episode, non-BluRay releases
/// are the only things this filter catches — BD packs legitimately show
/// up years later, and batch releases are ambiguous enough that we let
/// the rescore pass handle them.
///
/// Returns `true` to drop, `false` to keep. The BD check uses the cheap
/// filename heuristic so this runs pre-classification without touching
/// the DB.
fn is_likely_sequel_leak(
    result: &SearchResult,
    is_finished: bool,
    season_year: Option<i32>,
) -> bool {
    if !is_finished {
        return false;
    }
    let Some(air_year) = season_year else {
        return false;
    };
    if result.upload_timestamp <= 0 {
        return false;
    }
    let Some(upload_year) = upload_year_of(result.upload_timestamp) else {
        return false;
    };
    upload_year - air_year >= 2
        && !source::looks_like_bluray_filename(&result.title)
        && !result.is_batch
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
                if !ctx.allow_batch && result.is_batch {
                    continue;
                }
                if !matches_target(&result.title, ctx.aliases, ctx.target, ctx.expected_season, ctx.batch_episode_match && result.is_batch) {
                    continue;
                }
                if is_likely_sequel_leak(&result, ctx.is_finished, ctx.season_year) {
                    continue;
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
            if is_likely_sequel_leak(&result, ctx.is_finished, ctx.season_year) {
                continue;
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

pub fn matches_target(title: &str, aliases: &[String], target: &SearchTarget, expected_season: i32, allow_batch_episode: bool) -> bool {
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

#[allow(clippy::too_many_arguments)]
fn rescore_for_auto_search(
    result: &SearchResult,
    classification: &ClassificationResult,
    config: &Config,
    aliases: &[String],
    target: &SearchTarget,
    expected_season: i32,
    is_finished: bool,
    season_year: Option<i32>,
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

    // Date-based penalty for FINISHED series: if a result was uploaded long after
    // the series aired, it's likely for a sequel season rather than this one.
    // Exempt BD releases since those legitimately appear years later.
    if is_finished {
        if let Some(air_year) = season_year {
            let is_bluray = classification.source == Source::BluRay;
            if result.upload_timestamp > 0 && !is_bluray {
                if let Some(upload_year) = upload_year_of(result.upload_timestamp) {
                    let year_gap = upload_year - air_year;
                    // If uploaded 2+ years after the series aired, it's probably a sequel
                    if year_gap >= 2 {
                        score -= 80;
                    }
                }
            }
        }
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
        let good_release = "[Group] Main Title Subtitle One [BD 1080p].mkv";
        assert!(matches_target(
            good_release,
            &aliases,
            &SearchTarget::Single,
            0,
            false,
        ));
    }
}
