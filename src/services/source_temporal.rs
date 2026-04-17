//! Layer 4 — temporal inference.
//!
//! The weakest pre-download signal, used as a tiebreaker when the stronger
//! layers are ambiguous. Given AniList-style series status, a coarse airing
//! year, and whether the incoming release is a batch, emits at most one
//! weak [`SourceEvidence`]. The rules are:
//!
//! | Situation                                           | Inference       |
//! |-----------------------------------------------------|-----------------|
//! | Currently airing, single-episode release            | Web (0.75)      |
//! | Finished 1+ year ago, batch release                 | BluRay (0.65)   |
//! | Finished this year, single-episode release          | Web (0.70)      |
//! | Otherwise                                           | no signal       |
//!
//! The plan doc's original rules are written in months ("6+ months", "less
//! than 3 months"). We collapse the finer month granularity into year
//! comparisons so the layer can run off the year fields we actually store.
//! "Finished 1+ year ago" is the coarser version of "6+ months ago";
//! "finished this year" is the coarser version of "less than 3 months ago."
//! The confidences are kept low (0.65–0.75) so Layer 4 can only sway the
//! aggregator when L1 / L2 / L3 disagree or go missing.
//!
//! When computing "years since the show finished" the layer prefers the
//! explicit `end_year` and falls back to `season_year` (the start year) only
//! when no end date is known. The fallback isn't free: a show that started
//! in 2015 but ran for a decade would look like it's been off the air for
//! ten years when it actually wrapped last year, which would trip the
//! BluRay rule incorrectly. When the layer is forced to use the
//! start-year fallback it drops the emitted confidence to 0.55 so the
//! aggregator gives the signal less weight.
//!
//! This layer is a pure synchronous function with no I/O. It runs every
//! time a caller supplies [`SeriesContext`] — no cost gating, unlike Layer 2.
//!
//! This module does NOT fold evidence into a final decision — that's the
//! job of [`crate::services::source::aggregate`]. It emits at most one
//! weak piece of evidence.

use crate::services::source::{Origin, Source, SourceEvidence};

const ORIGIN: Origin = Origin::Temporal;

/// Reduced confidence used when the layer falls back to `season_year`
/// (i.e. no `end_year` was known). The start year is a noisy proxy for
/// "how long since the show ended" so we keep the signal deliberately
/// weak.
const FALLBACK_CONFIDENCE_SCALE: f32 = 0.55 / 0.65;

/// Compute a temporal inference evidence record, if the inputs warrant one.
///
/// - `status` — raw AniList-style status string (`"RELEASING"`,
///   `"FINISHED"`, `"CANCELLED"`, `"NOT_YET_RELEASED"`, `"HIATUS"`, …).
///   Matching is case-insensitive so callers can pass the DB column
///   directly.
/// - `season_year` — the year the series started airing, as stored in
///   AniList metadata. Used as a fallback when `end_year` is unknown.
/// - `end_year` — the year the series finished airing, when AniList or a
///   fallback provider carries an explicit end date. Preferred over
///   `season_year` for the finished-era rules because a long-running
///   series might have started a decade before it ended.
/// - `is_batch` — whether the incoming release is a season/batch pack (as
///   opposed to a single weekly episode).
/// - `today_year` — current calendar year, injected so tests are
///   deterministic.
///
/// Returns `None` when no rule matches, or exactly one weak evidence record
/// when a rule fires. Evidence emitted via the `season_year` fallback is
/// scaled down to 0.55 (for the BluRay rule) so the aggregator can still
/// override it with stronger signals.
pub fn classify_temporal(
    status: &str,
    season_year: Option<i32>,
    end_year: Option<i32>,
    is_batch: bool,
    today_year: i32,
) -> Option<SourceEvidence> {
    let status_norm = status.trim().to_ascii_uppercase();

    // Rule 1: currently airing + single episode → Web.
    // Simulcasts land on streaming first; a batch of a still-airing show
    // is a scene batch re-encode, not a BluRay pack, so we don't fire for
    // batches here.
    if status_norm == "RELEASING" && !is_batch {
        return Some(SourceEvidence::new(
            Source::Web,
            0.75,
            ORIGIN,
            "airing + single episode",
        ));
    }

    // AniList uses FINISHED / CANCELLED; Jikan normalizes "Finished Airing"
    // to FINISHED_AIRING. Accept all three so the finished-era rules still
    // fire when metadata fell back to Jikan.
    let is_finished = matches!(
        status_norm.as_str(),
        "FINISHED" | "FINISHED_AIRING" | "CANCELLED"
    );
    if !is_finished {
        return None;
    }

    // Prefer end_year (accurate "when did it finish?"). Fall back to
    // season_year (when did it *start*) only if we have nothing better —
    // downstream confidence is reduced to mark the estimate as weaker.
    let (reference_year, using_fallback) = match end_year {
        Some(y) => (y, false),
        None => (season_year?, true),
    };

    let years_since_end = today_year - reference_year;

    // Rule 2: finished 1+ year ago + batch release → BluRay.
    // Japanese home-video windows typically land 3–6 months after the TV
    // run ends. A batch grabbed a year or more after the end date is much
    // more likely to be a BD rip than a TV encode.
    if years_since_end >= 1 && is_batch {
        let confidence = if using_fallback { 0.65 * FALLBACK_CONFIDENCE_SCALE } else { 0.65 };
        let detail = if using_fallback {
            "finished 1+ year ago + batch (start-year fallback)"
        } else {
            "finished 1+ year ago + batch"
        };
        return Some(SourceEvidence::new(Source::BluRay, confidence, ORIGIN, detail));
    }

    // Rule 3: finished this year + single-episode release → Web.
    // A show that finished airing in the current calendar year is still
    // within the simulcast window; single episodes are almost certainly
    // streaming rips, not the still-unreleased BluRay.
    //
    // We only fire this when we have a real end_year — the start-year
    // fallback would misfire for the very long-running shows that benefit
    // most from the end_year plumbing in the first place (e.g. a show
    // that started in 2023 but is still airing "this year" in 2026 would
    // look like it "finished this year" if we squinted at season_year).
    if !using_fallback && years_since_end == 0 && !is_batch {
        return Some(SourceEvidence::new(
            Source::Web,
            0.70,
            ORIGIN,
            "finished this year + single episode",
        ));
    }

    None
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airing_single_episode_is_web() {
        let ev = classify_temporal("RELEASING", Some(2026), None, false, 2026).unwrap();
        assert_eq!(ev.source, Source::Web);
        assert!((ev.confidence - 0.75).abs() < 1e-4);
        assert_eq!(ev.origin, Origin::Temporal);
    }

    #[test]
    fn airing_status_is_case_insensitive() {
        assert!(classify_temporal("releasing", Some(2026), None, false, 2026).is_some());
        assert!(classify_temporal("Releasing", Some(2026), None, false, 2026).is_some());
        assert!(classify_temporal("  RELEASING  ", Some(2026), None, false, 2026).is_some());
    }

    #[test]
    fn airing_batch_release_emits_nothing() {
        // A batch for a still-airing show is unusual — almost always a
        // scene re-encode rather than a legitimate BluRay pack. Don't
        // fire the BluRay rule.
        assert!(classify_temporal("RELEASING", Some(2026), None, true, 2026).is_none());
    }

    #[test]
    fn finished_1_year_ago_batch_is_bluray() {
        let ev = classify_temporal("FINISHED", Some(2024), Some(2024), true, 2026).unwrap();
        assert_eq!(ev.source, Source::BluRay);
        assert!((ev.confidence - 0.65).abs() < 1e-4);
    }

    #[test]
    fn finished_many_years_ago_batch_is_bluray() {
        let ev = classify_temporal("FINISHED", Some(2015), Some(2015), true, 2026).unwrap();
        assert_eq!(ev.source, Source::BluRay);
    }

    #[test]
    fn cancelled_counts_as_finished() {
        // CANCELLED shows also get home-video releases (sometimes rushed
        // ones), and more importantly they're definitely not still airing.
        let ev = classify_temporal("CANCELLED", Some(2024), Some(2024), true, 2026).unwrap();
        assert_eq!(ev.source, Source::BluRay);
    }

    #[test]
    fn jikan_finished_airing_counts_as_finished() {
        // When metadata fell back from AniList to Jikan, `status` comes in
        // as "FINISHED_AIRING" (Jikan's normalized form of "Finished
        // Airing"). The rules must fire or the entire classification path
        // silently regresses on Jikan-fed series.
        let ev = classify_temporal("FINISHED_AIRING", Some(2024), Some(2024), true, 2026).unwrap();
        assert_eq!(ev.source, Source::BluRay);
    }

    #[test]
    fn finished_1_year_ago_single_episode_emits_nothing() {
        // The finished-recently+single rule is for this-year finales; a
        // year later the BluRay window is open but a single-episode grab
        // could be either source. No evidence.
        assert!(classify_temporal("FINISHED", Some(2024), Some(2024), false, 2026).is_none());
    }

    #[test]
    fn finished_this_year_single_episode_is_web() {
        let ev = classify_temporal("FINISHED", Some(2026), Some(2026), false, 2026).unwrap();
        assert_eq!(ev.source, Source::Web);
        assert!((ev.confidence - 0.70).abs() < 1e-4);
    }

    #[test]
    fn finished_this_year_batch_emits_nothing() {
        // Finale aired this year but someone posted a batch — could be an
        // early BD release or a collected streaming rip, so stay silent.
        assert!(classify_temporal("FINISHED", Some(2026), Some(2026), true, 2026).is_none());
    }

    #[test]
    fn unknown_status_emits_nothing() {
        assert!(classify_temporal("NOT_YET_RELEASED", Some(2026), None, false, 2026).is_none());
        assert!(classify_temporal("HIATUS", Some(2026), None, false, 2026).is_none());
        assert!(classify_temporal("", Some(2026), None, false, 2026).is_none());
        assert!(classify_temporal("gibberish", Some(2026), None, false, 2026).is_none());
    }

    #[test]
    fn finished_without_year_emits_nothing() {
        // No end_year *and* no season_year → we can't decide which
        // finished-era rule applies.
        assert!(classify_temporal("FINISHED", None, None, true, 2026).is_none());
        assert!(classify_temporal("FINISHED", None, None, false, 2026).is_none());
    }

    #[test]
    fn airing_without_year_still_fires() {
        // The airing rule doesn't need a year — status alone is enough.
        assert!(classify_temporal("RELEASING", None, None, false, 2026).is_some());
    }

    #[test]
    fn future_series_year_clamps_gracefully() {
        // A show with end_year in the future (e.g. 2027 on 2026-04-12)
        // shouldn't panic or trigger the 1+-year-ago rule.
        let ev = classify_temporal("FINISHED", Some(2027), Some(2027), true, 2026);
        assert!(ev.is_none());
    }

    #[test]
    fn evidence_records_include_readable_detail() {
        let ev = classify_temporal("RELEASING", Some(2026), None, false, 2026).unwrap();
        assert!(!ev.detail.is_empty());
        assert!(ev.detail.contains("airing"));
    }

    #[test]
    fn end_year_preferred_over_season_year() {
        // Long-running show: started 2015, wrapped 2025. The BluRay rule
        // fires (finished 1 year ago in 2026) with full confidence —
        // season_year alone would also fire, but at the reduced
        // fallback confidence.
        let ev = classify_temporal("FINISHED", Some(2015), Some(2025), true, 2026).unwrap();
        assert_eq!(ev.source, Source::BluRay);
        assert!((ev.confidence - 0.65).abs() < 1e-4,
            "end_year path should emit full confidence, got {}", ev.confidence);
    }

    #[test]
    fn season_year_fallback_uses_reduced_confidence() {
        // end_year unknown — the layer falls back to season_year and
        // scales confidence down to ~0.55.
        let ev = classify_temporal("FINISHED", Some(2024), None, true, 2026).unwrap();
        assert_eq!(ev.source, Source::BluRay);
        assert!((ev.confidence - 0.55).abs() < 1e-3,
            "fallback confidence should be ~0.55, got {}", ev.confidence);
        assert!(ev.detail.contains("fallback"));
    }

    #[test]
    fn fallback_does_not_fire_finished_this_year_web_rule() {
        // Without a real end_year we can't tell "finished this year" from
        // "started this year and still airing", so Rule 3 stays silent.
        assert!(classify_temporal("FINISHED", Some(2026), None, false, 2026).is_none());
    }
}
