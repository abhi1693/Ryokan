//! Custom Format + SeaDex overlay + auto-search rescoring.
//!
//! Three entry points, all `pub(super)` because mod.rs's orchestration
//! code is the sole caller:
//!
//! - [`apply_cf_seadex_overlay`] — takes a base score, applies the CF
//!   and SeaDex contributions, emits one tracing::debug! per candidate,
//!   and returns `Some(final)` or `None` if below the floor.
//! - [`format_scoring_detail`] — stable log-line format for the breakdown.
//! - [`rescore_for_auto_search`] — the base-score computation itself.

use std::collections::HashSet;

use crate::models::config::Config;
use crate::services::custom_formats::{self, CompiledCustomFormat, EvalContext};
use crate::services::nyaa::SearchResult;
use crate::services::quality;
use crate::services::seadex;
use crate::services::source::{self, ClassificationResult, Resolution, Source};

use super::{
    SearchTarget, normalize_title, parse_release_numbers, season_mismatch, token_overlap_ratio,
    token_set,
};

/// Apply the Custom Format + SeaDex overlay to a base score.
///
/// Returns `Some(final_score)` if the candidate survives the CF
/// minimum-score floor, or `None` if it should be dropped. The SeaDex
/// score bump is suppressed whenever the compiled CF set contains a
/// `SeaDexBestSpecification` — the user has taken ownership of that
/// number and double-counting would be a silent regression.
///
/// On the way through, emits one tracing::debug! line per candidate with
/// a CF-aware breakdown (plan §6.3). Operators who want to introspect
/// "why did X win / Y lose" can set
/// `RUST_LOG=ryokan::auto_search::scoring=debug`. The previous code
/// wrote to the DB log table here, but at 50-200 candidates per search
/// that was a sustained INSERT stream the `logs` UI flooded with rather
/// than aided.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_cf_seadex_overlay(
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
    // saturating_add at the combine — base, cf, and seadex_bonus are
    // each i32 and any one of them can be ±10k+. With ~22 CFs all
    // matching positively plus the 10k SeaDex boost plus base, naive
    // `+` can wrap to a large negative and silently demote every
    // candidate below minimum_score.
    let final_score = base.saturating_add(cf).saturating_add(seadex_bonus);

    let detail =
        format_scoring_detail(base, cf, &breakdown, seadex_bonus, final_score, below_floor);
    // tracing::debug! instead of logger::debug — 50-200 candidates per
    // search × one debug row each meant a sustained INSERT stream into
    // the `logs` table on every auto-search. Terminal/container logs
    // are the right surface for this granularity of detail; operators
    // who want it can set RUST_LOG=ryokan::auto_search=debug.
    tracing::debug!(
        target: "ryokan::auto_search::scoring",
        title = %result.title,
        "{}",
        detail
    );

    if below_floor { None } else { Some(final_score) }
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
    let mut out = format!("base={base}, {cf_section}, seadex={seadex_bonus}, final={final_score}");
    if below_floor {
        out.push_str(" DROPPED(below minimum_score floor)");
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(super) fn rescore_for_auto_search(
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
    absolute_offset: i32,
) -> i32 {
    let mut score = result.score;
    let lower = result.title.to_lowercase();
    let normalized_title = normalize_title(&result.title);
    let title_tokens = token_set(&normalized_title);

    let best_overlap = aliases
        .iter()
        .map(|alias| {
            let normalized_alias = normalize_title(alias);
            if normalized_title.contains(&normalized_alias) {
                1.0
            } else {
                token_overlap_ratio(&title_tokens, &token_set(&normalized_alias))
            }
        })
        .fold(0.0f32, f32::max);
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
            let parsed = parse_release_numbers(&result.title);
            let relative_match = parsed.contains(ep);
            let absolute_match =
                absolute_offset > 0 && parsed.contains(&ep.saturating_add(absolute_offset));
            if relative_match || absolute_match {
                score += 40;
            } else if absolute_offset > 0 && !parsed.is_empty() {
                // #30 — Phase 2 lets candidates through the franchise-alias
                // pass even when their parsed number doesn't match either
                // the relative or the absolute target. Those are false
                // positives (e.g. "[Asahi-Anime Land] Jujutsu Kaisen 04"
                // surfacing for a JJK S3 E9 absolute-56 target). Bury
                // them with a large penalty so they sort to the bottom
                // of the interactive list and drop below any realistic
                // auto-search `custom_format_minimum_score` floor,
                // without hard-rejecting in case the user actually wants
                // a different episode that the parser mis-reads.
                score -= 1000;
            } else if absolute_offset > 0 && parsed.is_empty() {
                // Unparseable episode number through the franchise pass
                // ("Jujutsu Kaisen 04" with no dash separator) — smaller
                // penalty than a wrong-number, since "can't tell" is
                // less clearly wrong than "explicitly wrong."
                score -= 500;
            }
        }
    }

    score += quality::preferred_group_bonus(
        &result.group,
        &quality::parse_group_list(&config.preferred_groups),
    );

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

#[cfg(test)]
mod tests {
    use super::*;

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
    #[test]
    fn format_scoring_detail_surfaces_seadex_bonus() {
        // SeaDex bonus is the only non-CF overlay; make sure it shows
        // up in the final line so the log reader can tell "SeaDex hit"
        // apart from "CF scoring pushed this above everything else."
        let breakdown = vec![("x265".to_string(), 300)];
        let s = format_scoring_detail(60, 300, &breakdown, 10000, 10360, false);
        assert_eq!(s, "base=60, cf=+300 [x265 +300], seadex=10000, final=10360");
    }
}
