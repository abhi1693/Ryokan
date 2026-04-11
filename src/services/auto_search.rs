use std::collections::HashSet;
use std::sync::LazyLock;

use regex_lite::Regex;

use crate::models::config::Config;
use crate::services::{anilist::AnimeDetail, media, nyaa::{self, SearchOptions, SearchResult}, quality};

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

#[derive(Debug, Clone, serde::Serialize)]
pub struct AutoSearchHit {
    pub target_label: String,
    pub release_title: String,
    pub release_group: String,
    pub quality_tier: String,
    pub url: String,
    pub score: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
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
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    _allow_batch: bool,
) -> Vec<SearchResult> {
    let queries = build_queries(detail, target);
    let aliases = collect_aliases(detail);
    let preferred_groups = quality::parse_group_list(&config.preferred_groups);
    let preferred_res = preferred_resolution_for_profile(&config.quality_profile);
    let is_finished = detail.status == "FINISHED";
    let finished_mode = quality::FinishedSeriesMode::from_str(&config.finished_series_quality);
    let preferred_tier = quality::QualityTier::from_str(&config.quality_profile);
    let cutoff_tier = quality::QualityTier::from_str(&config.quality_cutoff);

    let expected_season = infer_season_from_detail(detail);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    // Interactive search: allow batch results so user can see & pick them,
    // but filter by season and episode to avoid showing wrong-season results.
    run_queries_interactive(&queries, &aliases, &preferred_groups, &preferred_res, target, expected_season, is_finished, detail.season_year, &mut seen, &mut candidates).await;

    if !preferred_groups.is_empty() {
        let group_queries = build_group_queries(detail, target, &preferred_groups);
        run_queries_interactive(&group_queries, &aliases, &preferred_groups, &preferred_res, target, expected_season, is_finished, detail.season_year, &mut seen, &mut candidates).await;
    }

    for c in &mut candidates {
        c.score = rescore_for_auto_search(c, config, &aliases, target, expected_season, is_finished, detail.season_year, finished_mode, preferred_tier, cutoff_tier);
    }

    candidates.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    candidates
}

pub async fn find_best_for_target(
    detail: &AnimeDetail,
    config: &Config,
    target: &SearchTarget,
    allow_batch: bool,
) -> Option<SearchResult> {
    let queries = build_queries(detail, target);
    let aliases = collect_aliases(detail);
    let preferred_groups = quality::parse_group_list(&config.preferred_groups);
    let preferred_res = preferred_resolution_for_profile(&config.quality_profile);
    let is_finished = detail.status == "FINISHED";
    let finished_mode = quality::FinishedSeriesMode::from_str(&config.finished_series_quality);
    let preferred_tier = quality::QualityTier::from_str(&config.quality_profile);
    let cutoff_tier = quality::QualityTier::from_str(&config.quality_cutoff);

    let expected_season = infer_season_from_detail(detail);
    let mut seen = HashSet::new();
    let mut candidates: Vec<SearchResult> = Vec::new();

    let categories = quality::nyaa_categories_for_format(&detail.format, config.allow_non_english);

    // Phase 1: standard queries (alias + episode variants).
    run_queries(&queries, &aliases, &preferred_groups, &preferred_res, target, allow_batch, expected_season, is_finished, detail.season_year, &categories, &mut seen, &mut candidates).await;

    // Phase 2: if no candidate from a preferred group, try group-prefixed queries.
    let has_preferred_hit = !preferred_groups.is_empty()
        && candidates.iter().any(|c| {
            preferred_groups.iter().any(|g| g.eq_ignore_ascii_case(&c.group))
        });

    if !has_preferred_hit && !preferred_groups.is_empty() {
        let group_queries = build_group_queries(detail, target, &preferred_groups);
        run_queries(&group_queries, &aliases, &preferred_groups, &preferred_res, target, allow_batch, expected_season, is_finished, detail.season_year, &categories, &mut seen, &mut candidates).await;
    }

    // Phase 3: for finished series with BD preference, probe for BD releases.
    if is_finished && finished_mode != quality::FinishedSeriesMode::SameAsAiring {
        let has_bd_candidate = candidates.iter().any(|c| {
            quality::detect_tier(&c.title, &c.resolution).is_bluray()
        });

        if !has_bd_candidate {
            let bd_queries = quality::bd_probe_queries(&aliases);
            run_queries(&bd_queries, &aliases, &preferred_groups, &preferred_res, target, allow_batch, expected_season, is_finished, detail.season_year, &categories, &mut seen, &mut candidates).await;
        }
    }

    // Filter by finished-series quality mode.
    if is_finished && finished_mode == quality::FinishedSeriesMode::BdOnly {
        candidates.retain(|c| {
            let tier = quality::detect_tier(&c.title, &c.resolution);
            quality::passes_finished_filter(tier, finished_mode, true)
        });
    }

    // Rescore all candidates with quality-tier-aware scoring.
    for c in &mut candidates {
        c.score = rescore_for_auto_search(c, config, &aliases, target, expected_season, is_finished, detail.season_year, finished_mode, preferred_tier, cutoff_tier);
    }

    candidates.sort_by(|a, b| b.score.cmp(&a.score).then(b.seeders.cmp(&a.seeders)));
    candidates.into_iter().next()
}

/// Run a set of queries against Nyaa page 1, collecting valid candidates.
async fn run_queries(
    queries: &[String],
    aliases: &[String],
    preferred_groups: &[String],
    preferred_resolution: &str,
    target: &SearchTarget,
    allow_batch: bool,
    expected_season: i32,
    is_finished: bool,
    season_year: Option<i32>,
    categories: &[String],
    seen: &mut HashSet<String>,
    candidates: &mut Vec<SearchResult>,
) {
    for category in categories {
        for query in queries {
            let opts = SearchOptions {
                query: query.clone(),
                category: category.clone(),
                filter: "0".to_string(),
                user: String::new(),
                preferred_groups: preferred_groups.to_vec(),
                preferred_resolution: preferred_resolution.to_string(),
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
                if !allow_batch && result.is_batch {
                    continue;
                }
                if !matches_target(&result.title, aliases, target, expected_season) {
                    continue;
                }
                // For FINISHED series, reject non-BD results uploaded 2+ years after airing
                if is_finished {
                    if let Some(air_year) = season_year {
                        if result.upload_timestamp > 0 {
                            let upload_year = 1970 + (result.upload_timestamp / 31_536_000) as i32;
                            if upload_year - air_year >= 2 {
                                let tier = quality::detect_tier(&result.title, &result.resolution);
                                if !tier.is_bluray() && !result.is_batch {
                                    continue;
                                }
                            }
                        }
                    }
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
    aliases: &[String],
    preferred_groups: &[String],
    preferred_resolution: &str,
    target: &SearchTarget,
    expected_season: i32,
    is_finished: bool,
    season_year: Option<i32>,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<SearchResult>,
) {
    for query in queries {
        let opts = SearchOptions {
            query: query.clone(),
            category: "1_0".to_string(),
            filter: "0".to_string(),
            user: String::new(),
            preferred_groups: preferred_groups.to_vec(),
            preferred_resolution: preferred_resolution.to_string(),
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
            let alias_match = aliases.iter().any(|alias| {
                let normalized_alias = normalize_title(alias);
                normalized_title.contains(&normalized_alias)
                    || token_overlap_ratio(&title_tokens, &token_set(&normalized_alias)) >= 0.5
            });
            if !alias_match {
                continue;
            }
            // Season check: reject results clearly from a different season
            if season_mismatch(&result.title, expected_season) {
                continue;
            }
            // Episode check for single-episode targets (allow batches through)
            if let SearchTarget::Episode(target_ep) = target {
                if !result.is_batch {
                    let parsed = parse_release_numbers(&result.title);
                    if !parsed.is_empty() && !parsed.contains(target_ep) {
                        continue;
                    }
                }
            }
            // For FINISHED series, reject non-BD results uploaded 2+ years after airing
            if is_finished {
                if let Some(air_year) = season_year {
                    if result.upload_timestamp > 0 {
                        let upload_year = 1970 + (result.upload_timestamp / 31_536_000) as i32;
                        if upload_year - air_year >= 2 {
                            let tier = quality::detect_tier(&result.title, &result.resolution);
                            if !tier.is_bluray() && !result.is_batch {
                                continue;
                            }
                        }
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
    let mut monitored: Vec<i32> = monitored_episodes.iter().copied().collect();
    monitored.sort_unstable();
    monitored.dedup();

    monitored
        .into_iter()
        .filter(|ep| !existing.contains(ep))
        .map(SearchTarget::Episode)
        .collect()
}

/// Build upgrade targets: monitored episodes that exist on disk but are below
/// the quality cutoff. These are candidates for automatic quality upgrades.
pub fn build_upgrade_targets(
    disk_files: &[media::EpisodeFile],
    monitored_episodes: &[i32],
    cutoff_tier: quality::QualityTier,
    quality_tags: &std::collections::HashMap<i32, crate::models::episode_tags::EpisodeQualityTag>,
) -> Vec<(SearchTarget, quality::QualityTier)> {
    let monitored: HashSet<i32> = monitored_episodes.iter().copied().collect();
    let mut targets = Vec::new();
    for file in disk_files {
        if !monitored.contains(&file.episode_number) {
            continue;
        }
        // Prefer the quality tag recorded at grab time (from the release title)
        // over re-detecting from the renamed filename on disk.
        let existing_tier = if let Some(tag) = quality_tags.get(&file.episode_number) {
            let tier = quality::QualityTier::from_str(&tag.quality_tag);
            if tier != quality::QualityTier::Unknown { tier } else { quality::tier_from_disk_quality(&file.quality) }
        } else {
            quality::tier_from_disk_quality(&file.quality)
        };
        if existing_tier == quality::QualityTier::Unknown {
            continue;
        }
        if existing_tier.rank() < cutoff_tier.rank() {
            targets.push((
                SearchTarget::Episode(file.episode_number),
                existing_tier,
            ));
        }
    }
    targets.sort_by_key(|(t, _)| match t {
        SearchTarget::Episode(n) => *n,
        SearchTarget::Single => 0,
    });
    targets
}

pub fn target_label(target: &SearchTarget) -> String {
    match target {
        SearchTarget::Single => "Single".to_string(),
        SearchTarget::Episode(ep) => format!("Episode {}", ep),
    }
}

fn build_queries(detail: &AnimeDetail, target: &SearchTarget) -> Vec<String> {
    let aliases = collect_aliases(detail);
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

pub fn collect_aliases(detail: &AnimeDetail) -> Vec<String> {
    dedupe_strings(vec![
        detail.title_romaji.clone(),
        detail.title_english.clone(),
        detail.title_native.clone(),
    ])
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

pub fn matches_target(title: &str, aliases: &[String], target: &SearchTarget, expected_season: i32) -> bool {
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
            // If this looks like a batch (many episode numbers), reject for single-episode targets.
            // A range of 3+ episodes means this is a batch/multi-episode release.
            if parsed.len() > 2 {
                return false;
            }
            parsed.contains(target_ep)
        }
    }
}

fn rescore_for_auto_search(
    result: &SearchResult,
    config: &Config,
    aliases: &[String],
    target: &SearchTarget,
    expected_season: i32,
    is_finished: bool,
    season_year: Option<i32>,
    finished_mode: quality::FinishedSeriesMode,
    preferred_tier: quality::QualityTier,
    cutoff_tier: quality::QualityTier,
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
            let detected_tier = quality::detect_tier(&result.title, &result.resolution);
            if result.upload_timestamp > 0 && !detected_tier.is_bluray() {
                // Approximate the upload year from the timestamp
                let upload_year = 1970 + (result.upload_timestamp / 31_536_000) as i32;
                let year_gap = upload_year - air_year;
                // If uploaded 2+ years after the series aired, it's probably a sequel
                if year_gap >= 2 {
                    score -= 80;
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

    // Quality tier scoring.
    let detected_tier = quality::detect_tier(&result.title, &result.resolution);
    score += quality::tier_score(detected_tier, preferred_tier, cutoff_tier);

    // For finished series with BD preference, give BD releases a significant boost.
    if is_finished && finished_mode == quality::FinishedSeriesMode::PreferBd && detected_tier.is_bluray() {
        score += 35;
    }

    score
}

fn preferred_resolution_for_profile(profile: &str) -> String {
    match profile {
        "web_480" | "dvd" | "bd_480" => "480".to_string(),
        "web_720" | "bd_720" | "remux_720" => "720".to_string(),
        "web_1080" | "bd_1080" | "remux_1080" => "1080".to_string(),
        _ => "1080".to_string(),
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
