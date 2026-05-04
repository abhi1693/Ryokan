//! Property-based tests for pure-function invariants.
//!
//! Hand-picked unit tests pin specific behaviors at known inputs;
//! proptest fuzzes the input space and asserts invariants that should
//! hold for every input the strategies generate. Default 256 cases
//! per `proptest!` block — tunable via `PROPTEST_CASES` when iterating
//! on a flake (e.g. `PROPTEST_CASES=10000 cargo nextest run --test
//! proptest_invariants`).
//!
//! What goes here vs. inline `#[cfg(test)] mod tests`:
//!   * Inline tests pin specific (input, expected) pairs — quick to
//!     diagnose when one fails, fast in tight feedback loops.
//!   * Proptest invariants assert "for all inputs X satisfying Y, the
//!     output satisfies Z." Slower (256 cases × shrink) and surface
//!     bugs hand-picked tests miss (off-by-one at unrepresented
//!     boundaries, integer-overflow shapes, the input nobody thought
//!     to write down). They sit in `tests/` so a regression in the
//!     property is obviously a property failure, not a unit-test
//!     failure with a clever assertion.
//!
//! All tests target only the public API of the `ryokan` crate so this
//! file builds as a normal integration test.

use proptest::prelude::*;
use ryokan::services::nyaa::{SearchOptions, SearchResult};
use ryokan::services::scoring::{ScoreComponent, score_result_with_breakdown};
use ryokan::services::source::{
    self, ClassificationResult, DecisionRule, Resolution, Source, SourceEvidence, WebKind,
    aggregate, score_classification,
};

// ─── Helpers ──────────────────────────────────────────────────────

/// Minimal `SearchResult` builder. Other fields take their `Default`
/// values, keeping the strategy bodies focused on the inputs the
/// invariants care about.
fn search_result(seeders: i32, title: &str) -> SearchResult {
    SearchResult {
        title: title.to_string(),
        link: String::new(),
        magnet: String::new(),
        torrent: String::new(),
        size: String::new(),
        size_bytes: 0,
        seeders,
        leechers: 0,
        downloads: 0,
        group: String::new(),
        resolution: "1080p".to_string(),
        quality_label: String::new(),
        source: String::new(),
        web_kind: String::new(),
        is_remux: false,
        is_bdmv: false,
        is_batch: false,
        is_trusted: false,
        score: 0,
        info_hash: String::new(),
        score_breakdown: Vec::new(),
        upload_date: String::new(),
        indexer_id: None,
        indexer_name: String::new(),
    }
}

fn classification(source: Source, resolution: Resolution) -> ClassificationResult {
    ClassificationResult {
        source,
        resolution,
        is_remux: false,
        is_bdmv: false,
        web_kind: WebKind::Unknown,
        confidence: 1.0,
        needs_review: false,
        evidence: vec![],
        decision_rule: DecisionRule::Empty,
    }
}

/// proptest strategy for a `Source` that's never `Unknown`. Used in
/// monotonicity tests where `Unknown` would short-circuit the early
/// return at line 998 of scoring.rs and break the invariant we're
/// asserting (the special-case path returns -5 regardless of inputs).
fn known_source() -> impl Strategy<Value = Source> {
    prop_oneof![
        Just(Source::Tv),
        Just(Source::Hdtv),
        Just(Source::Dvd),
        Just(Source::Web),
        Just(Source::BluRay),
    ]
}

fn known_resolution() -> impl Strategy<Value = Resolution> {
    prop_oneof![
        Just(Resolution::R480p),
        Just(Resolution::R576p),
        Just(Resolution::R720p),
        Just(Resolution::R1080p),
        Just(Resolution::R2160p),
    ]
}

// ─── scoring invariants ───────────────────────────────────────────

proptest! {
    /// The breakdown's per-component deltas must sum to the returned
    /// total. This is the load-bearing invariant for the score-
    /// breakdown UI: if a row's components don't sum to the total,
    /// users get a visibly wrong "why this score" tooltip.
    ///
    /// The existing inline test pins this for four hand-picked shapes;
    /// proptest fuzzes the input space and is the backstop against
    /// future code paths that mutate `total` without `parts.push` or
    /// vice versa.
    #[test]
    fn breakdown_deltas_always_sum_to_total(
        seeders in any::<i32>(),
        downloads in any::<i32>(),
        size_bytes in any::<i64>(),
        is_batch in any::<bool>(),
        is_trusted in any::<bool>(),
        prefer_subs in any::<bool>(),
        // Bound title length and charset to keep the title parser /
        // anitomy / regex_lite work proportional. Unbounded random
        // bytes would spend most of each case in tokenization.
        title in "[a-zA-Z0-9 \\-\\[\\]\\(\\)\\.]{0,80}",
    ) {
        let mut r = search_result(seeders, &title);
        r.downloads = downloads;
        r.size_bytes = size_bytes;
        r.is_batch = is_batch;
        r.is_trusted = is_trusted;
        let opts = SearchOptions::default();
        let (total, parts) = score_result_with_breakdown(&r, &opts, prefer_subs);
        let sum: i32 = parts.iter().map(|c: &ScoreComponent| c.delta).sum();
        prop_assert_eq!(
            total,
            sum,
            "sum-of-deltas invariant violated for {:?}, parts={:?}",
            r.title,
            parts
        );
    }

    /// Seeders score band is monotonically non-decreasing in seeder
    /// count. More seeders should never produce a LOWER `Seeders`
    /// component delta (with all other inputs fixed). Pins the
    /// directional shape of the seeder ladder against the hand-picked
    /// boundary tests' exact values.
    #[test]
    fn seeders_band_is_monotonically_non_decreasing(
        a in 0_i32..=10_000,
        b in 0_i32..=10_000,
    ) {
        let r_a = search_result(a, "[G] Show - 01.mkv");
        let r_b = search_result(b, "[G] Show - 01.mkv");
        let opts = SearchOptions::default();
        let delta_a = score_result_with_breakdown(&r_a, &opts, true)
            .1
            .into_iter()
            .find(|c| c.label == "Seeders")
            .map(|c| c.delta)
            .unwrap_or(0);
        let delta_b = score_result_with_breakdown(&r_b, &opts, true)
            .1
            .into_iter()
            .find(|c| c.label == "Seeders")
            .map(|c| c.delta)
            .unwrap_or(0);
        if a >= b {
            prop_assert!(
                delta_a >= delta_b,
                "{} seeders ({}) should score >= {} seeders ({})",
                a, delta_a, b, delta_b
            );
        } else {
            prop_assert!(delta_a <= delta_b);
        }
    }
}

// ─── source classification invariants ────────────────────────────

proptest! {
    /// `aggregate` of an empty evidence vec must produce an Unknown
    /// classification — it's the documented base case the layered
    /// pipeline relies on for "no signal, fall through to whatever
    /// caller wants to do next." Hand-picked test pins the empty
    /// case; proptest catches the regression where some future code
    /// path passes a vec of all-zero-confidence entries (which is a
    /// different shape than empty but should arguably also produce
    /// Unknown — at least pin the truly-empty case here).
    #[test]
    fn aggregate_empty_evidence_is_always_unknown(
        // Drop in some unrelated state (boolean noise) so the test is
        // doing something proptest-flavored rather than a single
        // assertion on a constant input.
        _filler in any::<bool>(),
    ) {
        let result = aggregate(&[]);
        prop_assert_eq!(result.source, Source::Unknown);
        prop_assert_eq!(result.resolution, Resolution::Unknown);
    }

    /// `score_classification` peaks at the exact-match resolution.
    /// At fixed source + preferences, the resolution that exactly
    /// matches `preferred_resolution` must score >= every other
    /// resolution.
    ///
    /// This is the correct expression of the resolution-ladder shape:
    /// **strict monotonicity in resolution rank does NOT hold** because
    /// the scoring function deliberately adds a `+15` exact-match
    /// bonus at line 1010 of scoring code — so 1080p beats 2160p when
    /// 1080p is preferred (the user said "I want 1080p, not 4K"). My
    /// first attempt at this property asserted strict monotonicity and
    /// proptest correctly rejected it with the minimal counterexample
    /// `source=Tv, a=1080p, b=2160p` — preserving that finding here as
    /// a load-bearing comment so the next person doesn't try the same
    /// flawed property and remove the exact-match bonus to "fix it."
    #[test]
    fn at_preferred_resolution_never_scores_below_other_resolutions(
        source in known_source(),
        other in known_resolution(),
    ) {
        let preferred_source = Source::BluRay;
        let preferred_resolution = Resolution::R1080p;
        let cutoff_source = Source::Web;
        let cutoff_resolution = Resolution::R720p;

        let s_at = score_classification(
            &classification(source, preferred_resolution),
            preferred_source,
            preferred_resolution,
            cutoff_source,
            cutoff_resolution,
        );
        let s_other = score_classification(
            &classification(source, other),
            preferred_source,
            preferred_resolution,
            cutoff_source,
            cutoff_resolution,
        );
        prop_assert!(
            s_at >= s_other,
            "at-preferred {preferred_resolution:?} ({s_at}) must score >= {other:?} ({s_other}) at source {source:?}"
        );
    }

    /// `needs_review` is always a non-positive contribution. A release
    /// flagged for review must NEVER score higher than its confidently-
    /// classified twin (everything else equal). Pins the penalty
    /// direction.
    #[test]
    fn needs_review_never_scores_above_confident_twin(
        source in known_source(),
        resolution in known_resolution(),
        is_remux in any::<bool>(),
        is_bdmv in any::<bool>(),
    ) {
        let mut clean = classification(source, resolution);
        clean.is_remux = is_remux;
        clean.is_bdmv = is_bdmv;
        let mut review = clean.clone();
        review.needs_review = true;

        let preferred_source = Source::BluRay;
        let preferred_resolution = Resolution::R1080p;
        let cutoff_source = Source::Web;
        let cutoff_resolution = Resolution::R720p;

        let s_clean = score_classification(
            &clean,
            preferred_source,
            preferred_resolution,
            cutoff_source,
            cutoff_resolution,
        );
        let s_review = score_classification(
            &review,
            preferred_source,
            preferred_resolution,
            cutoff_source,
            cutoff_resolution,
        );
        prop_assert!(
            s_review <= s_clean,
            "needs_review must not score above the clean twin: review={s_review} clean={s_clean}",
        );
    }
}

// ─── ClassificationResult rank ladder ────────────────────────────

proptest! {
    /// `ClassificationResult.rank()` produces a tuple (resolution_rank,
    /// source_rank, bluray_tier, web_kind_rank). The documented contract
    /// is that resolution dominates source — a higher-resolution result
    /// always ranks above a lower-resolution one regardless of source.
    /// Pins that the tuple's lexicographic ordering matches.
    #[test]
    fn rank_tuple_is_lexicographically_resolution_first(
        a_source in known_source(),
        a_res in known_resolution(),
        b_source in known_source(),
        b_res in known_resolution(),
    ) {
        let a = classification(a_source, a_res);
        let b = classification(b_source, b_res);
        let ra = a.rank();
        let rb = b.rank();

        // The first tuple element is the resolution rank; if it
        // differs, the rank ordering must follow it regardless of
        // any other tuple element.
        if a_res.rank() > b_res.rank() {
            prop_assert!(ra > rb, "higher resolution must rank higher: a={ra:?} b={rb:?}");
        } else if a_res.rank() < b_res.rank() {
            prop_assert!(ra < rb);
        }
    }
}

// Suppress unused-import warning when proptest isn't the only consumer.
#[allow(dead_code)]
fn _import_check_source_evidence(_: SourceEvidence) {}

#[allow(dead_code)]
fn _import_check_source_module() {
    let _ = source::Resolution::R1080p;
}
