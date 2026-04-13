use serde::{Deserialize, Serialize};

/// What to do for finished series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishedSeriesMode {
    /// Same behavior as airing — grab best available per profile.
    SameAsAiring,
    /// Prefer BD: search for BD first, fall back to WEB if none found.
    PreferBd,
    /// Only grab BD or above — skip WEB entirely for finished series.
    BdOnly,
}

impl FinishedSeriesMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "prefer_bd" => FinishedSeriesMode::PreferBd,
            "bd_only" => FinishedSeriesMode::BdOnly,
            _ => FinishedSeriesMode::SameAsAiring,
        }
    }
}

/// Shared preferred-group scoring used by both RSS and auto-search.
/// `preferred_groups` should be ordered by priority (first = most preferred).
/// Returns a score bonus/penalty.
pub fn preferred_group_bonus(group: &str, preferred_groups: &[String]) -> i32 {
    if preferred_groups.is_empty() {
        return 0;
    }
    if group.trim().is_empty() {
        return -15;
    }
    for (idx, preferred) in preferred_groups.iter().enumerate() {
        if preferred.eq_ignore_ascii_case(group.trim()) {
            return 180 - (idx as i32 * 30);
        }
    }
    -40
}

/// Parse a comma-separated group list into a vec of trimmed, non-empty strings.
pub fn parse_group_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Return the Nyaa search categories for a given AniList format.
/// MUSIC → Anime Music Video (1_1) + Audio (2_0).
/// Everything else → a single category determined by `allow_non_english`:
///   false → English-translated (1_2), true → Anime All (1_0).
pub fn nyaa_categories_for_format(format: &str, allow_non_english: bool) -> Vec<String> {
    if format == "MUSIC" {
        vec!["1_1".to_string(), "2_0".to_string()]
    } else if allow_non_english {
        vec!["1_0".to_string()]
    } else {
        vec!["1_2".to_string()]
    }
}

/// Build Nyaa search queries to probe for BD releases of a series.
pub fn bd_probe_queries(aliases: &[String]) -> Vec<String> {
    let mut queries = Vec::new();
    for alias in aliases {
        queries.push(format!("{} bluray", alias));
        queries.push(format!("{} BD", alias));
        queries.push(format!("{} BDRip", alias));
        queries.push(format!("{} remux", alias));
    }
    queries
}

/// Build Nyaa search queries that target batch/complete releases.
///
/// For popular shows with active weekly seeders, Nyaa page 1 under a
/// plain title query is dominated by recent single-episode releases —
/// batches get pushed off the first page entirely. Adding explicit
/// "batch" / "complete" / "01-" keywords funnels the Nyaa search toward
/// listings with those tokens in the title and surfaces batch releases
/// that a generic query would never reach on page 1.
pub fn batch_probe_queries(aliases: &[String]) -> Vec<String> {
    let mut queries = Vec::new();
    for alias in aliases {
        queries.push(format!("{} batch", alias));
        queries.push(format!("{} complete", alias));
        // "X 01-" catches the common "01-12", "01~24" batch naming.
        queries.push(format!("{} 01-", alias));
    }
    queries
}
