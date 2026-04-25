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

#[cfg(test)]
mod tests {
    //! Coverage for the small pure helpers shared across RSS scoring
    //! and auto-search. Each function lives on a hot path (every
    //! candidate scored fires `preferred_group_bonus` once;
    //! `parse_group_list` runs every Settings save), but none had
    //! unit tests before this commit.
    use super::*;

    // ── FinishedSeriesMode::from_str ──────────────────────────────────

    #[test]
    fn finished_mode_from_str_canonical_values() {
        assert_eq!(
            FinishedSeriesMode::from_str("prefer_bd"),
            FinishedSeriesMode::PreferBd
        );
        assert_eq!(
            FinishedSeriesMode::from_str("bd_only"),
            FinishedSeriesMode::BdOnly
        );
        assert_eq!(
            FinishedSeriesMode::from_str("same_as_airing"),
            FinishedSeriesMode::SameAsAiring
        );
    }

    #[test]
    fn finished_mode_from_str_unknown_falls_back_to_same_as_airing() {
        // Defensive default: if a future settings key drifts, finished
        // series get treated like airing rather than silently
        // tightening to BD-only (which would unilaterally stop
        // grabbing WEB releases).
        assert_eq!(
            FinishedSeriesMode::from_str(""),
            FinishedSeriesMode::SameAsAiring
        );
        assert_eq!(
            FinishedSeriesMode::from_str("garbage"),
            FinishedSeriesMode::SameAsAiring
        );
    }

    // ── preferred_group_bonus ─────────────────────────────────────────

    #[test]
    fn preferred_group_bonus_no_preference_returns_zero() {
        // Empty preferred list = "user has no preference" — every
        // group scores 0 from this dimension.
        assert_eq!(preferred_group_bonus("SubsPlease", &[]), 0);
        assert_eq!(preferred_group_bonus("", &[]), 0);
    }

    #[test]
    fn preferred_group_bonus_orders_top_to_bottom() {
        // The bonus formula is `180 - idx*30`. Pin every step so a
        // refactor that touches the slope or starting point is caught.
        let groups: Vec<String> = (0..7).map(|i| format!("Group{i}")).collect();
        assert_eq!(preferred_group_bonus("Group0", &groups), 180);
        assert_eq!(preferred_group_bonus("Group1", &groups), 150);
        assert_eq!(preferred_group_bonus("Group2", &groups), 120);
        assert_eq!(preferred_group_bonus("Group3", &groups), 90);
        assert_eq!(preferred_group_bonus("Group4", &groups), 60);
        assert_eq!(preferred_group_bonus("Group5", &groups), 30);
        // The 7th preferred group hits exactly zero — still in the
        // preferred list but contributes nothing. Past that, the
        // formula gives negative values; users with overflowing lists
        // see "preferred but with a discount."
        assert_eq!(preferred_group_bonus("Group6", &groups), 0);
    }

    #[test]
    fn preferred_group_bonus_unknown_group_in_non_empty_list_is_penalty() {
        // The user picked preferences and this group isn't in them.
        // A penalty pushes the candidate down vs. unstated-preference
        // baseline.
        let groups = vec!["MTBB".to_string(), "Erai-raws".to_string()];
        assert_eq!(preferred_group_bonus("RandomGroup", &groups), -40);
    }

    #[test]
    fn preferred_group_bonus_empty_group_with_non_empty_preferences_is_small_penalty() {
        // Group field empty (Nyaa scraper couldn't parse a `[Group]`
        // bracket) hits a softer penalty than "unknown group" — a
        // missing tag is more often a parse miss than a "this is from
        // a fly-by-night encoder" signal.
        let groups = vec!["MTBB".to_string()];
        assert_eq!(preferred_group_bonus("", &groups), -15);
        assert_eq!(preferred_group_bonus("   ", &groups), -15);
    }

    #[test]
    fn preferred_group_bonus_match_is_case_insensitive() {
        // Real `[Group]` brackets are sometimes capitalized
        // differently across releases — `[Subsplease]` vs
        // `[SubsPlease]`. The match must collapse case so the
        // user's preferences don't whiplash on capitalization drift.
        let groups = vec!["MTBB".to_string()];
        assert_eq!(preferred_group_bonus("mtbb", &groups), 180);
        assert_eq!(preferred_group_bonus("MtBb", &groups), 180);
        assert_eq!(preferred_group_bonus("MTBB", &groups), 180);
    }

    #[test]
    fn preferred_group_bonus_trims_whitespace_on_group_input() {
        // The Nyaa scraper occasionally emits trailing spaces inside
        // bracket tokens. Don't let whitespace sabotage a match.
        let groups = vec!["MTBB".to_string()];
        assert_eq!(preferred_group_bonus("  MTBB  ", &groups), 180);
    }

    // ── parse_group_list ──────────────────────────────────────────────

    #[test]
    fn parse_group_list_splits_and_trims() {
        assert_eq!(
            parse_group_list("MTBB,Erai-raws,SubsPlease"),
            vec!["MTBB", "Erai-raws", "SubsPlease"]
        );
        // Spaces around the commas are tolerated — Settings UI doesn't
        // strip them and we don't want to bounce the user back for a
        // formatting nit.
        assert_eq!(
            parse_group_list("MTBB, Erai-raws , SubsPlease"),
            vec!["MTBB", "Erai-raws", "SubsPlease"]
        );
    }

    #[test]
    fn parse_group_list_drops_empty_segments() {
        // Trailing comma, double comma, leading comma — all common
        // copy-paste artifacts. Every shape collapses to the same
        // empty-segments-removed list.
        assert_eq!(
            parse_group_list("MTBB,,Erai-raws,"),
            vec!["MTBB", "Erai-raws"]
        );
        assert_eq!(parse_group_list(",MTBB"), vec!["MTBB"]);
        assert!(parse_group_list("").is_empty());
        assert!(parse_group_list(",,,").is_empty());
        // Whitespace-only segments collapse too.
        assert!(parse_group_list("   ,  , ").is_empty());
    }

    // ── nyaa_categories_for_format ────────────────────────────────────

    #[test]
    fn nyaa_categories_music_returns_amv_plus_audio() {
        // MUSIC format covers AMV releases (1_1) and audio rips
        // (2_0) — both can be relevant for music-format AL entries.
        // `allow_non_english` is irrelevant for the MUSIC branch.
        for allow in [false, true] {
            assert_eq!(
                nyaa_categories_for_format("MUSIC", allow),
                vec!["1_1".to_string(), "2_0".to_string()]
            );
        }
    }

    #[test]
    fn nyaa_categories_default_branches_on_allow_non_english() {
        // Non-MUSIC formats funnel into a single category. The flag
        // toggles between "English-translated only" (1_2) and
        // "Anime All including raws" (1_0).
        assert_eq!(
            nyaa_categories_for_format("TV", false),
            vec!["1_2".to_string()]
        );
        assert_eq!(
            nyaa_categories_for_format("TV", true),
            vec!["1_0".to_string()]
        );
        assert_eq!(
            nyaa_categories_for_format("MOVIE", false),
            vec!["1_2".to_string()]
        );
        assert_eq!(
            nyaa_categories_for_format("OVA", true),
            vec!["1_0".to_string()]
        );
    }

    #[test]
    fn nyaa_categories_unknown_format_treated_as_default() {
        // Empty / unrecognized format strings land on the non-MUSIC
        // branch. Better to overscan than to return an empty
        // category list (which would silently disable RSS for that
        // series).
        assert_eq!(
            nyaa_categories_for_format("", false),
            vec!["1_2".to_string()]
        );
    }

    // ── bd_probe_queries / batch_probe_queries ────────────────────────

    #[test]
    fn bd_probe_queries_emits_four_per_alias() {
        // Each alias generates four queries (`bluray`, `BD`, `BDRip`,
        // `remux`). Two aliases → 8 queries. The set is small enough
        // to assert verbatim.
        assert!(bd_probe_queries(&[]).is_empty());
        let q = bd_probe_queries(&["Frieren".to_string()]);
        assert_eq!(
            q,
            vec![
                "Frieren bluray",
                "Frieren BD",
                "Frieren BDRip",
                "Frieren remux",
            ]
        );
    }

    #[test]
    fn batch_probe_queries_emits_three_per_alias() {
        // `batch`, `complete`, `01-`. The "01-" tail matches the
        // common batch naming `01-12`, `01~24` etc.
        assert!(batch_probe_queries(&[]).is_empty());
        let q = batch_probe_queries(&["Frieren".to_string()]);
        assert_eq!(q, vec!["Frieren batch", "Frieren complete", "Frieren 01-"]);
    }

    #[test]
    fn probe_queries_cross_product_with_multiple_aliases() {
        // Both helpers run alias-major: every alias gets every probe.
        // Two aliases × three probes = six queries from
        // `batch_probe_queries`.
        let aliases = vec!["A".to_string(), "B".to_string()];
        assert_eq!(batch_probe_queries(&aliases).len(), 6);
        assert_eq!(bd_probe_queries(&aliases).len(), 8);
    }
}
