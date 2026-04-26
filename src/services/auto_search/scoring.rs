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
use crate::services::scoring::ScoreComponent;
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
    apply_cf_seadex_overlay_with_breakdown(
        base,
        result,
        classification,
        cfs,
        seadex_hashes,
        seadex_boost_enabled,
        minimum_score,
    )
    .map(|(score, _)| score)
}

/// Same as [`apply_cf_seadex_overlay`] but also returns the per-CF and
/// SeaDex breakdown entries so the caller can fold them into the
/// `SearchResult`'s `score_breakdown` for UI display. Used by the
/// interactive search path where each candidate's breakdown needs to
/// stay in sync with its final score.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_cf_seadex_overlay_with_breakdown(
    base: i32,
    result: &SearchResult,
    classification: &ClassificationResult,
    cfs: &[CompiledCustomFormat],
    seadex_hashes: &HashSet<String>,
    seadex_boost_enabled: bool,
    minimum_score: i32,
) -> Option<(i32, Vec<ScoreComponent>)> {
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

    if below_floor {
        return None;
    }

    let mut components: Vec<ScoreComponent> = breakdown
        .into_iter()
        .map(|(name, delta)| ScoreComponent {
            label: format!("CF: {name}"),
            delta,
            detail: None,
        })
        .collect();
    if seadex_bonus != 0 {
        components.push(ScoreComponent {
            label: "SeaDex Best".to_string(),
            delta: seadex_bonus,
            detail: Some("release flagged isBest by releases.moe".to_string()),
        });
    }

    Some((final_score, components))
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
    rescore_for_auto_search_with_breakdown(
        result,
        classification,
        config,
        aliases,
        target,
        expected_season,
        is_finished,
        finished_mode,
        preferred_source,
        preferred_resolution,
        cutoff_source,
        cutoff_resolution,
        absolute_offset,
        false, // batch_search_mode — non-batch callers
    )
    .0
}

/// Same as [`rescore_for_auto_search`] but also returns the list of
/// score components added on top of the scraper's base score. Used by
/// the interactive search path so each candidate's breakdown in the UI
/// stays in sync with its final displayed score.
///
/// `batch_search_mode` is `true` when the caller is explicitly
/// collecting batch-only candidates (`collect_scored_batches_for_
/// target`, powering both interactive batch search and auto-search's
/// batch grab path). In that mode the single-target batch penalty is
/// suppressed — penalizing a batch for being a batch when the user
/// explicitly asked for batches is nonsense, and surfaced in the
/// breakdown as a confusing "-5 Batch Penalty" on every row.
#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
pub(super) fn rescore_for_auto_search_with_breakdown(
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
    batch_search_mode: bool,
) -> (i32, Vec<ScoreComponent>) {
    let mut score = result.score;
    let mut parts: Vec<ScoreComponent> = Vec::new();
    let mut add =
        |parts: &mut Vec<ScoreComponent>, label: &str, delta: i32, detail: Option<String>| {
            if delta == 0 {
                return;
            }
            score += delta;
            parts.push(ScoreComponent {
                label: label.to_string(),
                delta,
                detail,
            });
        };
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
    let overlap_delta = (best_overlap * 40.0) as i32;
    add(
        &mut parts,
        "Title Alias Match",
        overlap_delta,
        Some(format!(
            "{:.0}% of best alias tokens matched",
            best_overlap * 100.0
        )),
    );

    // Season mismatch penalty (explicit season markers like S03, "3rd Season")
    if season_mismatch(&result.title, expected_season) {
        add(
            &mut parts,
            "Season Mismatch",
            -100,
            Some(format!("release season ≠ expected S{expected_season:02}")),
        );
    }

    match target {
        SearchTarget::Single => {
            // Movie / Special / OVA bonus and Batch penalty both
            // assume the user is looking for a single-unit target.
            // In explicit batch-search mode every candidate is a batch
            // for the same series, so both signals are meaningless
            // and would uniformly lift or lower the whole slate. Gate
            // both on `!batch_search_mode` so batch-grab rankings are
            // driven by quality + alias match + seeders, not by the
            // presence of "Movie" / "OVA" keywords in the batch title.
            if !batch_search_mode
                && (lower.contains("movie") || lower.contains("special") || lower.contains("ova"))
            {
                add(&mut parts, "Movie / Special / OVA", 8, None);
            }
            if result.is_batch && !batch_search_mode {
                add(&mut parts, "Batch Penalty (single target)", -5, None);
            }
        }
        SearchTarget::Episode(ep) => {
            if result.is_batch {
                if !batch_search_mode {
                    // Same gate the SearchTarget::Single arm uses: the
                    // user explicitly asked for batches when batch_search_mode
                    // is on, so penalizing every candidate -20 for being a
                    // batch is uniform across the slate (doesn't change
                    // ranking) but produces a confusing "Batch Penalty
                    // (episode target): single-episode grab preferred"
                    // line on every row in the breakdown. Suppress.
                    add(
                        &mut parts,
                        "Batch Penalty (episode target)",
                        -20,
                        Some("single-episode grab preferred".to_string()),
                    );
                }
            } else {
                add(&mut parts, "Single-Episode Target", 10, None);
            }
            let parsed = parse_release_numbers(&result.title);
            let relative_match = parsed.contains(ep);
            let absolute_match =
                absolute_offset > 0 && parsed.contains(&ep.saturating_add(absolute_offset));
            if relative_match || absolute_match {
                add(
                    &mut parts,
                    "Episode Number Match",
                    40,
                    Some(format!("release covers episode {ep}")),
                );
            } else if absolute_offset > 0 && !parsed.is_empty() {
                // #30 — franchise-alias fallback surfaces candidates
                // whose parsed number matches neither target. Bury them.
                add(
                    &mut parts,
                    "Wrong Episode Number",
                    -1000,
                    Some("franchise-pass release doesn't match target ep".to_string()),
                );
            } else if absolute_offset > 0 && parsed.is_empty() {
                add(
                    &mut parts,
                    "Unparseable Episode Number",
                    -500,
                    Some("franchise-pass release with no parseable ep".to_string()),
                );
            }
        }
    }

    let group_bonus = quality::preferred_group_bonus(
        &result.group,
        &quality::parse_group_list(&config.preferred_groups),
    );
    add(&mut parts, "Preferred Group (auto)", group_bonus, None);

    // Classification-aware quality scoring.
    let classification_delta = source::score_classification(
        classification,
        preferred_source,
        preferred_resolution,
        cutoff_source,
        cutoff_resolution,
    );
    add(
        &mut parts,
        "Source / Resolution Fit",
        classification_delta,
        Some(format!(
            "{} {}",
            classification.source.as_str(),
            classification.resolution.as_str()
        )),
    );

    // For finished series with BD preference, give BD releases a significant boost.
    if is_finished
        && finished_mode == quality::FinishedSeriesMode::PreferBd
        && classification.source == Source::BluRay
    {
        add(
            &mut parts,
            "Finished Series BD Bonus",
            35,
            Some("finished series + prefer_bd + BluRay source".to_string()),
        );
    }

    (score, parts)
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
