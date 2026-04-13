// Phase 1a foundation: nothing in production calls into this module yet — it
// is exercised only by unit tests until Phase 1b wires the classifier into
// `auto_search`, `rss`, and `upgrade`. Remove this allow when that happens.
#![allow(dead_code)]

//! Source classification types, signal aggregation, and the pre-download
//! orchestrator.
//!
//! This module defines the primitive types used by the classification pipeline
//! and the aggregator that folds layer evidence into a final result. Individual
//! layer implementations live in sibling modules (`source_filename`,
//! `source_groups`, and — in later phases — `source_ffprobe`, `source_dir`,
//! `source_description`, `source_temporal`).
//!
//! The pipeline is split into two phases:
//! - **Pre-download** (layers 1–4): runs against a torrent title before the
//!   grab decision. Cheap, filename+DB only. Entry point:
//!   [`classify_release`].
//! - **Post-download** (layers 5–6): runs against the on-disk file after
//!   import. Reads container metadata via ffprobe and walks the directory.
//!
//! Both phases produce the same [`ClassificationResult`] type, so the two
//! phases can confirm/override each other via the same aggregator.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::SqlitePool;

use crate::services::source_filename::classify_filename;
use crate::services::source_groups::classify_group;

// ───────────────────────────────────────────────────────────────────────────
// Source
// ───────────────────────────────────────────────────────────────────────────

/// Source of a release, independent of resolution.
///
/// `rank()` gives a monotonic ordering for "better source." Ordering is
/// deliberately `Unknown < Tv < Hdtv < Dvd < Web < BluRay` — this matches
/// common anime community preferences where BD encodes are considered the
/// canonical reference and WEB sits a notch below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Source {
    Unknown,
    Tv,
    Hdtv,
    Dvd,
    Web,
    BluRay,
}

impl Source {
    pub fn rank(self) -> u8 {
        match self {
            Source::Unknown => 0,
            Source::Tv => 1,
            Source::Hdtv => 2,
            Source::Dvd => 3,
            Source::Web => 4,
            Source::BluRay => 5,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Unknown => "Unknown",
            Source::Tv => "TV",
            Source::Hdtv => "HDTV",
            Source::Dvd => "DVD",
            Source::Web => "Web",
            Source::BluRay => "BluRay",
        }
    }

    /// Parse from the string form stored in the DB. Case-insensitive.
    /// Returns `Source::Unknown` for unrecognized values.
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "tv" => Source::Tv,
            "hdtv" => Source::Hdtv,
            "dvd" => Source::Dvd,
            "web" | "webdl" | "web-dl" | "webrip" => Source::Web,
            "bluray" | "blu-ray" | "bd" | "bdrip" | "bdremux" => Source::BluRay,
            _ => Source::Unknown,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Resolution
// ───────────────────────────────────────────────────────────────────────────

/// Resolution tier, independent of source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Resolution {
    Unknown,
    R480p,
    R576p,
    R720p,
    R1080p,
    R2160p,
}

impl Resolution {
    pub fn rank(self) -> u8 {
        match self {
            Resolution::Unknown => 0,
            Resolution::R480p => 1,
            Resolution::R576p => 2,
            Resolution::R720p => 3,
            Resolution::R1080p => 4,
            Resolution::R2160p => 5,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::Unknown => "Unknown",
            Resolution::R480p => "480p",
            Resolution::R576p => "576p",
            Resolution::R720p => "720p",
            Resolution::R1080p => "1080p",
            Resolution::R2160p => "2160p",
        }
    }

    /// Parse from a resolution string. Accepts both bare numbers ("1080") and
    /// suffixed forms ("1080p", "1080i"). Returns `Resolution::Unknown` on
    /// unrecognized input.
    pub fn from_str(s: &str) -> Self {
        let trimmed = s.trim().trim_end_matches(['p', 'i', 'P', 'I']);
        match trimmed {
            "2160" | "4k" | "4K" | "UHD" | "uhd" => Resolution::R2160p,
            "1080" => Resolution::R1080p,
            "720" => Resolution::R720p,
            "576" => Resolution::R576p,
            "480" => Resolution::R480p,
            _ => Resolution::Unknown,
        }
    }

    /// Derive resolution from explicit pixel dimensions. Used by ffprobe layer
    /// in Phase 2, and by Layer 1 for cross-referencing against mentioned
    /// dimensions like "1920x1080".
    pub fn from_dimensions(width: u32, height: u32) -> Self {
        // Allow slight variance for anamorphic / cropped content.
        if height >= 2000 {
            Resolution::R2160p
        } else if height >= 1000 {
            Resolution::R1080p
        } else if height >= 700 {
            Resolution::R720p
        } else if height >= 560 {
            Resolution::R576p
        } else if height >= 460 {
            Resolution::R480p
        } else {
            // DVD-native dimensions as a special case.
            if (width, height) == (720, 480) || (width, height) == (704, 480) {
                Resolution::R480p
            } else if (width, height) == (720, 576) {
                Resolution::R576p
            } else {
                Resolution::Unknown
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Evidence & classification result
// ───────────────────────────────────────────────────────────────────────────

/// A single piece of evidence from one classification layer.
///
/// Layers emit zero or more of these, and the aggregator folds them into a
/// final [`ClassificationResult`]. The `origin` and `detail` fields are
/// preserved end-to-end so the final result carries an audit trail that gets
/// logged when `needs_review` is true.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceEvidence {
    pub source: Source,
    pub confidence: f32,
    pub origin: &'static str,
    pub detail: String,
}

impl SourceEvidence {
    pub fn new(source: Source, confidence: f32, origin: &'static str, detail: impl Into<String>) -> Self {
        Self {
            source,
            confidence: confidence.clamp(0.0, 1.0),
            origin,
            detail: detail.into(),
        }
    }
}

/// The output of a classification run. Carries the final (source, resolution,
/// remux) decision plus the full evidence trail for auditing.
#[derive(Debug, Clone, Serialize)]
pub struct ClassificationResult {
    pub source: Source,
    pub resolution: Resolution,
    pub is_remux: bool,
    /// Confidence of the winning source decision (0.0–1.0). This is either
    /// the confidence of the dominant single signal or, for multi-signal
    /// decisions, the fraction of total evidence mass backing the winner.
    pub confidence: f32,
    /// True when the aggregator couldn't make a confident decision — either
    /// no signal reached threshold, or two sources had comparable strong
    /// signals pointing in different directions. UI should surface these
    /// files for manual tagging.
    pub needs_review: bool,
    /// Full evidence trail. Logged at DEBUG always, logged at INFO when
    /// `needs_review` is true.
    pub evidence: Vec<SourceEvidence>,
}

impl ClassificationResult {
    /// Empty "unknown" result. Useful as a fallback when no layer produced
    /// any evidence at all.
    pub fn unknown() -> Self {
        Self {
            source: Source::Unknown,
            resolution: Resolution::Unknown,
            is_remux: false,
            confidence: 0.0,
            needs_review: true,
            evidence: Vec::new(),
        }
    }

    /// Ranking tuple for comparison. Higher = better quality. Resolution
    /// dominates the ordering so a Web-1080p outranks a BluRay-720p, matching
    /// the priorities of Ryokan's existing quality tier enum.
    pub fn rank(&self) -> (u8, u8, u8) {
        (
            self.resolution.rank(),
            self.source.rank(),
            if self.is_remux { 1 } else { 0 },
        )
    }

    /// Human-readable label for logs and UI.
    pub fn label(&self) -> String {
        let remux = if self.is_remux { " Remux" } else { "" };
        match (self.source, self.resolution) {
            (Source::Unknown, Resolution::Unknown) => "Unknown".to_string(),
            (s, Resolution::Unknown) => s.as_str().to_string(),
            (Source::Unknown, r) => r.as_str().to_string(),
            (s, r) => format!("{}{} {}", s.as_str(), remux, r.as_str()),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Aggregator
// ───────────────────────────────────────────────────────────────────────────

/// Strong-signal threshold: any single evidence at or above this confidence
/// wins outright without further arbitration.
const STRONG_THRESHOLD: f32 = 0.90;

/// Minimum lead of the winner over runner-up when adding up evidence. If the
/// top two are within this gap, it's treated as conflicting and flagged for
/// review rather than silently picking the winner.
const MIN_LEAD: f32 = 0.30;

/// Any signal above this threshold is considered "strong enough that a
/// conflict matters." Used by the review-detection rule.
const CONFLICT_THRESHOLD: f32 = 0.70;

/// Minimum total evidence mass to avoid falling back to Unknown.
const MIN_TOTAL: f32 = 0.50;

/// Fold a list of evidence into a final classification result.
///
/// Rules, applied in order:
/// 1. If any single evidence has `confidence >= STRONG_THRESHOLD (0.90)`,
///    that source wins immediately. This lets high-confidence filename or
///    ffprobe signals bypass the full aggregation path.
/// 2. Otherwise, sum confidences per source across all evidence. The source
///    with the highest total wins, **but only if** it leads the runner-up by
///    at least `MIN_LEAD (0.30)`.
/// 3. If there's no clear lead but at least one evidence ≥ `CONFLICT_THRESHOLD
///    (0.70)` disagrees with the top source, flag `needs_review = true` while
///    still returning the best guess.
/// 4. If the total evidence mass for the best source is below `MIN_TOTAL
///    (0.50)`, classify as `Source::Unknown` with `needs_review = true`.
/// 5. Otherwise: return the winner with `needs_review = false`.
///
/// Note: this function does not set `resolution` or `is_remux`. Those come
/// from the individual layer outputs (resolution is observed, not aggregated)
/// and are set by the orchestrating `classify_*` functions. The returned
/// result has `resolution = Unknown` and `is_remux = false`; callers
/// overwrite those fields.
pub fn aggregate(evidence: &[SourceEvidence]) -> ClassificationResult {
    if evidence.is_empty() {
        return ClassificationResult::unknown();
    }

    // Rule 1: strong single-signal shortcut. Among strong signals, pick the
    // highest-confidence one; ties broken by source rank.
    if let Some(strong) = evidence
        .iter()
        .filter(|e| e.confidence >= STRONG_THRESHOLD)
        .max_by(|a, b| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.source.rank().cmp(&b.source.rank()))
        })
    {
        return ClassificationResult {
            source: strong.source,
            resolution: Resolution::Unknown,
            is_remux: false,
            confidence: strong.confidence,
            needs_review: false,
            evidence: evidence.to_vec(),
        };
    }

    // Rule 2: sum per source.
    let mut sums: HashMap<Source, f32> = HashMap::new();
    for e in evidence {
        *sums.entry(e.source).or_insert(0.0) += e.confidence;
    }

    let mut ranked: Vec<(Source, f32)> = sums.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.rank().cmp(&a.0.rank()))
    });

    let (leader, leader_sum) = ranked[0];
    let runner_sum = ranked.get(1).map(|(_, s)| *s).unwrap_or(0.0);
    let lead = leader_sum - runner_sum;

    // Rule 4: total mass too weak, fall back to Unknown.
    if leader_sum < MIN_TOTAL {
        return ClassificationResult {
            source: Source::Unknown,
            resolution: Resolution::Unknown,
            is_remux: false,
            confidence: leader_sum,
            needs_review: true,
            evidence: evidence.to_vec(),
        };
    }

    // Rule 3: detect strong conflict. If runner-up has a signal ≥
    // CONFLICT_THRESHOLD and the lead is small, flag for review.
    let has_strong_conflict = evidence
        .iter()
        .any(|e| e.confidence >= CONFLICT_THRESHOLD && e.source != leader);
    let needs_review = has_strong_conflict && lead < MIN_LEAD;

    // If the lead is large enough, it's a clean win regardless of conflicts.
    // If the lead is small but no strong conflict, still call it a win.
    ClassificationResult {
        source: leader,
        resolution: Resolution::Unknown,
        is_remux: false,
        // Normalize the confidence to a 0..1 range based on the leader's
        // share of total evidence mass. A leader with 1.5 out of 2.0 total
        // evidence gets 0.75; a dominant leader with 1.8 out of 2.0 gets 0.9.
        confidence: leader_sum / (leader_sum + runner_sum).max(leader_sum),
        needs_review,
        evidence: evidence.to_vec(),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Orchestrator
// ───────────────────────────────────────────────────────────────────────────

/// Run the pre-download classification pipeline against a release title.
///
/// Fuses Layer 1 (filename tokens via anitomy) with Layer 3 (release group
/// identity table) and returns the aggregated result. The post-download
/// layers (ffprobe, directory walk, description scraping) are not wired in
/// yet — they'll append additional evidence via the same aggregator in later
/// phases.
///
/// `resolution_hint` is an externally-supplied resolution string (typically
/// the `resolution` column scraped from a Nyaa listing). It is **only**
/// consulted for the resolution field when Layer 1 couldn't determine one
/// from the title tokens itself; it never participates in source aggregation.
/// This keeps the provenance of the two resolution sources distinct, so a
/// listing whose header claims 1080p while the title parses to 720p keeps
/// the title's value.
pub async fn classify_release(
    db: &SqlitePool,
    title: &str,
    resolution_hint: Option<&str>,
) -> ClassificationResult {
    let filename = classify_filename(title);

    let mut evidence = filename.evidence.clone();
    if let Some(group) = filename.release_group.as_deref() {
        if let Some(group_ev) = classify_group(db, group).await {
            evidence.push(group_ev);
        }
    }

    let mut result = aggregate(&evidence);

    // Resolution is observed, not aggregated. Prefer the filename parse —
    // it looked at the actual title tokens — and only fall back to the
    // external hint when Layer 1 came up empty.
    result.resolution = if filename.resolution != Resolution::Unknown {
        filename.resolution
    } else if let Some(hint) = resolution_hint {
        Resolution::from_str(hint)
    } else {
        Resolution::Unknown
    };
    result.is_remux = filename.is_remux;

    result
}

// ───────────────────────────────────────────────────────────────────────────
// Sync classification helpers
// ───────────────────────────────────────────────────────────────────────────

/// Classify a release using filename evidence only (no DB lookup, no group
/// identity layer). Used by on-disk upgrade detection where we want to
/// classify many already-tagged episodes without doing one DB round-trip
/// per episode.
///
/// This is a weaker signal than [`classify_release`] — it can't see group
/// identity — but it's entirely synchronous and produces a result of the
/// same shape so callers can compare ranks directly.
pub fn classify_release_sync(title: &str, resolution_hint: Option<&str>) -> ClassificationResult {
    use crate::services::source_filename::classify_filename;
    let filename = classify_filename(title);
    let mut result = aggregate(&filename.evidence);
    result.resolution = if filename.resolution != Resolution::Unknown {
        filename.resolution
    } else if let Some(hint) = resolution_hint {
        Resolution::from_str(hint)
    } else {
        Resolution::Unknown
    };
    result.is_remux = filename.is_remux;
    result
}

/// Rehydrate a `ClassificationResult` from already-stored DB columns, e.g.
/// `episode_quality_tags`. Used by upgrade detection to compare an on-disk
/// episode's persisted classification against the incoming release without
/// re-parsing the original title.
///
/// The evidence trail is not stored in the DB, so the returned result has
/// an empty `evidence` vec. That's fine for rank comparison — evidence is
/// only consumed by auditing code paths.
pub fn classification_from_stored(
    source: &str,
    resolution: &str,
    is_remux: bool,
    confidence: f32,
    needs_review: bool,
) -> ClassificationResult {
    ClassificationResult {
        source: Source::from_str(source),
        resolution: Resolution::from_str(resolution),
        is_remux,
        confidence,
        needs_review,
        evidence: Vec::new(),
    }
}

/// Build a synthetic `ClassificationResult` representing the user's quality
/// cutoff. Used so upgrade-detection can compare real releases against the
/// cutoff using the same rank tuple comparison as everything else.
pub fn cutoff_classification(cutoff_source: Source, cutoff_resolution: Resolution) -> ClassificationResult {
    ClassificationResult {
        source: cutoff_source,
        resolution: cutoff_resolution,
        is_remux: false,
        confidence: 1.0,
        needs_review: false,
        evidence: Vec::new(),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Finished-series filters
// ───────────────────────────────────────────────────────────────────────────

/// Filename-only BluRay heuristic. Runs Layer 1 (anitomy tokens) and the
/// aggregator without touching the DB, then checks whether the dominant
/// source is BluRay. Used by inline collection-time filters that need a
/// yes/no answer before the full DB-backed `classify_release` pass runs.
///
/// This is intentionally a weaker signal than `classify_release` — it can't
/// see group identity — but it's cheap enough to run inline during collection.
pub fn looks_like_bluray_filename(title: &str) -> bool {
    use crate::services::source_filename::classify_filename;
    let filename = classify_filename(title);
    // If the filename tokens alone already resolve to BluRay, that's a yes.
    // Otherwise, aggregate the evidence just in case (an ambiguous pile of
    // weak BluRay signals might still aggregate to BluRay).
    let agg = aggregate(&filename.evidence);
    agg.source == Source::BluRay
}

/// Does this classification pass a "BluRay only" finished-series filter?
/// Unknown sources are allowed through so that releases we couldn't confidently
/// classify aren't silently filtered out — they'll still compete on score.
pub fn passes_bd_only_filter(c: &ClassificationResult) -> bool {
    matches!(c.source, Source::BluRay | Source::Unknown)
}

// ───────────────────────────────────────────────────────────────────────────
// Scoring
// ───────────────────────────────────────────────────────────────────────────

/// Score a classification result against the user's preferred/cutoff
/// source+resolution. Returns a score delta to be added to the baseline from
/// `scoring::score_result_with_sub_pref`.
///
/// The two dimensions (source and resolution) are scored separately and
/// summed, with resolution carrying a heavier weight because it dominates the
/// ranking tuple in `ClassificationResult::rank`.
///
/// Scoring rules:
/// - Unknown on both dimensions → small penalty, don't filter out.
/// - At or above preferred → bonus, exact match earns extra.
/// - Below preferred → penalty proportional to the gap.
/// - At or above cutoff → small bonus.
/// - Remux is a small premium modifier when BluRay is the preferred source.
/// - `needs_review` classifications get a small penalty so confident
///   alternatives are preferred when available.
pub fn score_classification(
    c: &ClassificationResult,
    preferred_source: Source,
    preferred_resolution: Resolution,
    cutoff_source: Source,
    cutoff_resolution: Resolution,
) -> i32 {
    // Completely unknown release — keep the legacy -5 behavior.
    if c.source == Source::Unknown && c.resolution == Resolution::Unknown {
        return -5;
    }

    let mut score: i32 = 0;

    // ── Resolution (dominates) ────────────────────────────────────────────
    if c.resolution != Resolution::Unknown {
        let det = c.resolution.rank() as i32;
        let pref = preferred_resolution.rank() as i32;
        if det >= pref {
            score += 25;
            if det == pref {
                score += 15;
            }
        } else {
            let gap = pref - det;
            score -= 10 + gap * 10;
        }
        if det >= cutoff_resolution.rank() as i32 {
            score += 10;
        }
    } else {
        score -= 10;
    }

    // ── Source ────────────────────────────────────────────────────────────
    if c.source != Source::Unknown {
        let det = c.source.rank() as i32;
        let pref = preferred_source.rank() as i32;
        if det >= pref {
            score += 15;
            if det == pref {
                score += 10;
            }
        } else {
            let gap = pref - det;
            score -= 5 + gap * 5;
        }
        if det >= cutoff_source.rank() as i32 {
            score += 5;
        }
    } else {
        score -= 5;
    }

    // ── Remux premium ─────────────────────────────────────────────────────
    // Remux is only a bonus when the user actually wants BluRay-grade
    // fidelity. Penalizing it otherwise would be wrong (it's still a valid
    // BluRay source, just overkill), so we simply don't reward it.
    if c.is_remux && preferred_source.rank() >= Source::BluRay.rank() {
        score += 5;
    }

    // ── Needs-review penalty ──────────────────────────────────────────────
    // Prefer confidently-classified releases when they exist. Small value so
    // it can't flip a clearly better release.
    if c.needs_review {
        score -= 3;
    }

    score
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(src: Source, conf: f32, origin: &'static str) -> SourceEvidence {
        SourceEvidence::new(src, conf, origin, "")
    }

    #[test]
    fn rank_ordering_matches_expectations() {
        // Source ordering.
        assert!(Source::BluRay.rank() > Source::Web.rank());
        assert!(Source::Web.rank() > Source::Dvd.rank());
        assert!(Source::Dvd.rank() > Source::Hdtv.rank());
        assert!(Source::Hdtv.rank() > Source::Tv.rank());

        // Resolution ordering.
        assert!(Resolution::R2160p.rank() > Resolution::R1080p.rank());
        assert!(Resolution::R1080p.rank() > Resolution::R720p.rank());
    }

    #[test]
    fn classification_rank_prefers_resolution_over_source() {
        // Web-1080p should outrank BluRay-720p (resolution dominates).
        let web_1080 = ClassificationResult {
            source: Source::Web,
            resolution: Resolution::R1080p,
            is_remux: false,
            confidence: 1.0,
            needs_review: false,
            evidence: vec![],
        };
        let bd_720 = ClassificationResult {
            source: Source::BluRay,
            resolution: Resolution::R720p,
            is_remux: false,
            confidence: 1.0,
            needs_review: false,
            evidence: vec![],
        };
        assert!(web_1080.rank() > bd_720.rank());
    }

    #[test]
    fn aggregate_empty_evidence_is_unknown() {
        let result = aggregate(&[]);
        assert_eq!(result.source, Source::Unknown);
        assert!(result.needs_review);
    }

    #[test]
    fn aggregate_rule_1_strong_signal_wins_immediately() {
        // Single strong signal ≥ 0.90 short-circuits the rest.
        let evidence = vec![
            ev(Source::Web, 0.95, "filename"),
            ev(Source::BluRay, 0.40, "group"),
            ev(Source::BluRay, 0.30, "temporal"),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web);
        assert!(!result.needs_review);
        // Confidence should be the strong signal's own confidence.
        assert!((result.confidence - 0.95).abs() < 1e-4);
    }

    #[test]
    fn aggregate_rule_2_highest_sum_wins_with_clear_lead() {
        // No single strong signal, but two weak Web signals outweigh one
        // moderate BluRay signal by more than MIN_LEAD.
        let evidence = vec![
            ev(Source::Web, 0.60, "filename"),
            ev(Source::Web, 0.55, "group"),
            ev(Source::BluRay, 0.50, "temporal"),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web);
        assert!(!result.needs_review);
    }

    #[test]
    fn aggregate_rule_3_conflict_flags_for_review() {
        // Runner-up has a signal ≥ CONFLICT_THRESHOLD (0.70) and the lead is
        // less than MIN_LEAD (0.30) — should pick a winner but flag for
        // review.
        let evidence = vec![
            ev(Source::Web, 0.75, "filename"),
            ev(Source::BluRay, 0.70, "ffprobe"),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web); // Higher sum wins.
        assert!(result.needs_review, "conflict should trigger review");
    }

    #[test]
    fn aggregate_rule_4_all_weak_falls_to_unknown() {
        // Every signal below MIN_TOTAL (0.50) — total mass too weak.
        let evidence = vec![
            ev(Source::Web, 0.20, "temporal"),
            ev(Source::BluRay, 0.15, "temporal"),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Unknown);
        assert!(result.needs_review);
    }

    #[test]
    fn aggregate_clean_win_despite_weak_conflict() {
        // Strong leader, weak opposing signal — no review needed.
        let evidence = vec![
            ev(Source::Web, 0.95, "filename"),
            ev(Source::BluRay, 0.30, "temporal"),
        ];
        let result = aggregate(&evidence);
        assert_eq!(result.source, Source::Web);
        assert!(!result.needs_review);
    }
}
