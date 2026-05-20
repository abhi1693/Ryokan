//! Primitive classification types for the source pipeline: enums
//! (`Source`, `WebKind`, `Resolution`, `Origin`, `DecisionRule`) and the
//! aggregator's input/output records (`SourceEvidence`,
//! `ClassificationResult`). Re-exported from `services::source` so callers
//! keep using the familiar path.

use serde::Serialize;

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
            "bluray" | "blu-ray" | "bd" | "bdrip" | "bdremux" | "bdmv" => Source::BluRay,
            _ => Source::Unknown,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// WebKind — Sonarr-style sub-classification within the Web source family
// ───────────────────────────────────────────────────────────────────────────

/// Distinguishes the two flavors of `Source::Web` that Sonarr's quality
/// definitions treat separately:
///
/// - **WebDl** — direct download from the streaming platform's CDN (the
///   stream's actual source file). Higher fidelity, no re-encode.
/// - **WebRip** — captured/re-encoded from the streaming player. Lower
///   fidelity than WebDl.
///
/// `Unknown` is the fallback when the release tag was just bare "WEB" with
/// no further qualifier — common for older listings and a lot of fan
/// releases. A bare-"WEB" release is treated as if neither variant is
/// strongly indicated, so the rank tuple sees it slot between WebRip and
/// WebDl rather than overriding either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Default)]
pub enum WebKind {
    #[default]
    Unknown,
    WebRip,
    WebDl,
}

impl WebKind {
    /// Monotonic ordering: WebDl > WebRip > Unknown. Used by `rank()` so
    /// the top-level ranking tuple can prefer a WEB-DL over a WEBRip when
    /// both have the same `Source::Web`/resolution.
    pub fn rank(self) -> u8 {
        match self {
            WebKind::Unknown => 0,
            WebKind::WebRip => 1,
            WebKind::WebDl => 2,
        }
    }

    /// Display string used in episode labels and persisted to the DB.
    /// Empty for `Unknown` so it doesn't pollute legacy bare-"WEB" labels.
    pub fn as_str(self) -> &'static str {
        match self {
            WebKind::Unknown => "",
            WebKind::WebRip => "WEBRip",
            WebKind::WebDl => "WEB-DL",
        }
    }

    /// Parse from the string form stored in the DB. Accepts both the
    /// canonical Sonarr forms and a few common variants.
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "webdl" | "web-dl" | "web.dl" => WebKind::WebDl,
            "webrip" | "web-rip" | "web.rip" => WebKind::WebRip,
            _ => WebKind::Unknown,
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

    /// Parse from Sonarr's `ResolutionSpecification` integer value. Sonarr
    /// stores the raw pixel height (480, 576, 720, 1080, 2160), with `0 =
    /// Unknown`. Sonarr also defines `360` and `540` variants; Ryokan
    /// folds both to `Unknown` since we don't classify them as distinct
    /// tiers and anime releases at those heights are rare.
    ///
    /// Used by the Custom Formats parser (`services/custom_formats/parser.rs`)
    /// to compile a `ResolutionSpecification` into a comparison against
    /// `ClassificationResult::resolution`.
    pub fn from_int(value: i32) -> Self {
        match value {
            480 => Resolution::R480p,
            576 => Resolution::R576p,
            720 => Resolution::R720p,
            1080 => Resolution::R1080p,
            2160 => Resolution::R2160p,
            _ => Resolution::Unknown,
        }
    }

    /// Derive resolution from explicit pixel dimensions. Used by ffprobe layer
    /// in Phase 2, and by Layer 1 for cross-referencing against mentioned
    /// dimensions like "1920x1080".
    ///
    /// The height thresholds are loose lower-bounds to absorb anamorphic and
    /// cropped content — NTSC DVD (480), PAL DVD (576), and all the common
    /// anamorphic 720p variants land inside the same brackets, so no separate
    /// exact-dimension fallbacks are needed.
    pub fn from_dimensions(_width: u32, height: u32) -> Self {
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
            Resolution::Unknown
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
///
/// # `confidence` semantics
///
/// The `confidence` field is **not a probability**. It's a relative
/// weight in the aggregator's per-source sum — how much this piece of
/// evidence contributes to the "did source X win?" decision. Think of
/// it as evidence *mass*, clamped to `[0.0, 1.0]`.
///
/// Calibration per layer:
///
/// - **L1 filename** (source_filename): 0.30 — 0.95. Generic keyword
///   hits stay around 0.30 – 0.55; scene tags like `WEB-DL` or
///   `BDRip` sit at 0.70 – 0.85; an unambiguous group-and-keyword
///   match can reach 0.90+.
/// - **L2 Nyaa description** (source_nyaa): 0.60 — 0.90. Description
///   scrapes are relatively reliable when present, so the layer
///   emits mid-to-high values; ambiguous matches get dropped instead
///   of being emitted weakly.
/// - **L3 release group** (source_groups): 0.85. The group identity
///   table is curated and unambiguous per entry, so every emitted
///   record carries the same strong weight.
/// - **L4 temporal** (source_temporal): 0.55 — 0.75. Deliberately
///   weak — a tiebreaker, not a primary signal. The 0.55 band is the
///   `season_year` fallback path (see that module's doc comment);
///   0.65 and 0.75 are the end_year-backed rules.
/// - **L5 ffprobe** (source_ffprobe): up to 1.00. This is ground
///   truth — the file itself — and carries a special interpretation
///   in the aggregator: it can veto the lead via Rule 5 even when
///   the rest of the evidence disagrees.
/// - **L6 directory walk** (source_dir): 0.80 — 0.95. Disc markers
///   are high confidence; the weaker Specials/Extras/Bonus rule
///   drops to 0.80 because user-organized libraries sometimes mimic
///   the structure.
///
/// Aggregator thresholds (`STRONG_THRESHOLD = 0.90`, `MIN_TOTAL = 0.50`,
/// `MIN_LEAD = 0.15`, `ORIGIN_MAX = 1.3`) are calibrated against these
/// bands. Changing a layer's emission range without re-checking the
/// aggregator thresholds can silently shift the classification
/// behavior — bump the confidence of a weak layer too high and it
/// starts overriding stronger signals.
/// Which classification layer produced a piece of evidence. Replaces
/// the previous `&'static str` so a typo at the per-layer constant or
/// a comparison in the aggregator becomes a compile error instead of
/// silently demoting the layer's vote (the per-origin clamp and rule-1
/// tie-break key off this exact value).
///
/// Serialised as the lowercase variant name for on-disk JSON
/// compatibility with the previous string form, so existing
/// `episode_quality_tags.classification_evidence` rows render
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Filename,
    Description,
    Group,
    Temporal,
    Ffprobe,
    Dir,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Filename => "filename",
            Origin::Description => "description",
            Origin::Group => "group",
            Origin::Temporal => "temporal",
            Origin::Ffprobe => "ffprobe",
            Origin::Dir => "dir",
        }
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceEvidence {
    pub source: Source,
    /// Relative weight this evidence record contributes to the
    /// aggregator sum. **Not** a probability — see the struct-level
    /// docs for per-layer calibration bands and why the specific
    /// values matter.
    pub confidence: f32,
    pub origin: Origin,
    pub detail: String,
}

impl SourceEvidence {
    pub fn new(source: Source, confidence: f32, origin: Origin, detail: impl Into<String>) -> Self {
        Self {
            source,
            confidence: confidence.clamp(0.0, 1.0),
            origin,
            detail: detail.into(),
        }
    }
}

/// Tagged identifier for the aggregator rule that produced a given result.
///
/// Stored on the `ClassificationResult` directly by `aggregate()` instead
/// of being reverse-engineered after the fact. This makes the firing rule
/// authoritative (rather than a heuristic inference that could drift out of
/// sync with the rule bodies) and survives serialization so downstream
/// logging/audit code can read it without re-examining the evidence.
///
/// Variants mirror the rules documented on `aggregate()`:
/// - `Empty` — no evidence at all; fell back to `ClassificationResult::unknown()`.
/// - `Rule1Strong` — every strong signal agreed on the winner and rule 1's
///   short-circuit fired.
/// - `Rule2Sum` — clean per-source sum win with no conflict flag raised.
/// - `Rule3Weak` — total evidence mass below `MIN_TOTAL`; fell back to Unknown.
/// - `Rule4Conflict` — rule 4 flagged `needs_review` because a strong
///   runner-up was within `MIN_LEAD` of the leader.
/// - `Rule5GroundTruthVeto` — rule 5 vetoed the lead because an
///   ffprobe/dir observation at `STRONG_THRESHOLD` disagreed with the
///   aggregator's winning source. Takes precedence over Rule4Conflict in
///   logging because it's the more specific and more actionable cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub enum DecisionRule {
    #[default]
    Empty,
    Rule1Strong,
    Rule2Sum,
    Rule3Weak,
    Rule4Conflict,
    Rule5GroundTruthVeto,
}

impl DecisionRule {
    /// Short log-friendly identifier for the rule. Matches the strings the
    /// previous `decision_rule()` helper returned so log consumers don't
    /// need to learn new vocabulary.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Rule1Strong => "rule1-strong",
            Self::Rule2Sum => "rule2-sum",
            Self::Rule3Weak => "rule3-weak",
            Self::Rule4Conflict => "rule4-conflict",
            Self::Rule5GroundTruthVeto => "rule5-veto",
        }
    }
}

/// The output of a classification run. Carries the final (source, resolution,
/// remux) decision plus the full evidence trail for auditing.
#[derive(Debug, Clone, Serialize)]
pub struct ClassificationResult {
    pub source: Source,
    pub resolution: Resolution,
    /// True for BluRay **Remux** releases — a lossless MKV-wrapped extract
    /// of the disc's video+audio bitstreams with the BDMV container
    /// stripped. Distinct from `is_bdmv`: a remux is the canonical
    /// re-mux into MKV, while a BDMV/BD-Raw release ships the whole disc
    /// structure intact. The two flags are mutually exclusive in the
    /// label and are treated as different tiers by `rank()`.
    pub is_remux: bool,
    /// Sub-classification within the Web source family (WebDl vs WebRip).
    /// Always `WebKind::Unknown` when `source` isn't `Source::Web`.
    #[serde(default)]
    pub web_kind: WebKind,
    /// True for raw BDMV / BD-RAW disc-structure releases — the actual
    /// `BDMV/STREAM/*.m2ts` folder layout (or full ISO), with menus,
    /// multi-track audio, and chapter info intact. Implies
    /// `source == Source::BluRay`. **Distinct from `is_remux`**: a BDMV
    /// release is the unaltered disc, while a Remux is a lossless
    /// container-swap into MKV. They form three separate BluRay tiers:
    /// plain encode < Remux < BDMV.
    #[serde(default)]
    pub is_bdmv: bool,
    /// Confidence of the winning source decision (0.0–1.0).
    ///
    /// **Not** a calibrated probability — it's a compressed view of
    /// how much more evidence the winning source had than the
    /// runner-up, scaled so that a lopsided win approaches 1.0 and a
    /// close call approaches 0.5:
    ///
    /// ```text
    /// confidence = winner_mass / max(winner_mass + runner_up_mass, winner_mass)
    /// ```
    ///
    /// A single unopposed signal gets `confidence == 1.0`. Two signals
    /// with masses 1.8 and 0.2 give ~0.9. Two evenly-matched signals
    /// at 1.0 each give 0.5. The value is meant for UI display and
    /// ordering, not for statistical reasoning — downstream code that
    /// needs to know "how strong was this decision" should look at
    /// `needs_review` and `decision_rule` first.
    ///
    /// When `decision_rule == DecisionRule::Rule3Weak` (fallback to
    /// Unknown because total evidence mass was below `MIN_TOTAL`) this
    /// field carries the raw total evidence mass instead of the ratio
    /// — it's always below `MIN_TOTAL` in that case and callers that
    /// care distinguish the branches via `decision_rule`.
    pub confidence: f32,
    /// True when the aggregator couldn't make a confident decision — either
    /// no signal reached threshold, or two sources had comparable strong
    /// signals pointing in different directions. UI should surface these
    /// files for manual tagging.
    pub needs_review: bool,
    /// Full evidence trail. Logged at DEBUG always, logged at INFO when
    /// `needs_review` is true.
    pub evidence: Vec<SourceEvidence>,
    /// Which aggregator rule produced this result. Set authoritatively
    /// inside `aggregate()` at the matching branch, so downstream code
    /// never needs to reverse-engineer it from the other fields. Defaults
    /// to `DecisionRule::Empty` on constructors that don't run through
    /// the aggregator (e.g. `unknown()`, `cutoff_classification`, and
    /// `classification_from_stored_full`, which are all synthesized or
    /// rehydrated results rather than live classifications).
    #[serde(default)]
    pub decision_rule: DecisionRule,
}

impl ClassificationResult {
    /// Empty "unknown" result. Useful as a fallback when no layer produced
    /// any evidence at all.
    pub fn unknown() -> Self {
        Self {
            source: Source::Unknown,
            resolution: Resolution::Unknown,
            is_remux: false,
            web_kind: WebKind::Unknown,
            is_bdmv: false,
            confidence: 0.0,
            needs_review: true,
            evidence: Vec::new(),
            decision_rule: DecisionRule::Empty,
        }
    }

    /// Ranking tuple for comparison. Higher = better quality. Resolution
    /// dominates the ordering so a Web-1080p outranks a BluRay-720p,
    /// matching the priorities of Ryokan's existing quality tier enum.
    ///
    /// The fourth and fifth tuple slots break sub-source ties:
    /// - `bluray_tier()` orders the three BluRay variants:
    ///   plain encode (0) < Remux (1) < BDMV/BD-Raw (2). BDMV is the
    ///   highest tier because it preserves the disc structure as
    ///   shipped — Remux strips the BDMV container, and a plain encode
    ///   transcodes the video.
    /// - `web_kind.rank()` lets WEB-DL outrank WEBRip at the same
    ///   resolution.
    pub fn rank(&self) -> (u8, u8, u8, u8) {
        (
            self.resolution.rank(),
            self.source.rank(),
            self.bluray_tier(),
            self.web_kind.rank(),
        )
    }

    /// Combined BluRay sub-tier: 0 = plain encode, 1 = Remux, 2 = BDMV.
    /// BDMV wins over Remux when both flags are set on the same row,
    /// since the disc structure is strictly more information than the
    /// MKV-wrapped extract.
    pub fn bluray_tier(&self) -> u8 {
        if self.is_bdmv {
            2
        } else if self.is_remux {
            1
        } else {
            0
        }
    }

    /// Human-readable label for logs and UI. Sonarr-style `SOURCE-RES`
    /// base with an optional space-separated BluRay sub-tier suffix:
    /// `BD-1080p`, `BD-1080p Remux`, `BD-1080p RAW`, `WEB-1080p`,
    /// `WEBRip-1080p`, `HDTV-1080p`, `DVD-480p`, etc.
    ///
    /// Keeping the resolution adjacent to the source (rather than
    /// appending it after the sub-tier) preserves parity with Sonarr's
    /// quality definition names, which treat `<source>-<resolution>`
    /// as the immutable key and bolt any Remux qualifier on afterward.
    ///
    /// Source rendering is variant-aware:
    /// - `Source::Web` renders as bare `"WEB"` regardless of whether
    ///   the internal `web_kind` is `WebDl` or `Unknown` — most users
    ///   don't care about the distinction and seeing `WEBDL-1080p` on
    ///   some releases but `WEB-1080p` on others (based on whether the
    ///   filename happened to carry the token) produced more confusion
    ///   than information (issue #48). The `WebRip` variant DOES render
    ///   as `"WEBRip"` because that's the lower-quality sub-tier power
    ///   users want to spot. `web_kind` is still tracked internally so
    ///   Sonarr Custom Format `SourceSpecification` value 3 (WebDl)
    ///   still matches releases with explicit `WEB-DL` tokens.
    /// - `Source::BluRay` always renders as `BD` with at most one
    ///   trailing space-separated qualifier, mutually exclusive:
    ///   ` RAW` if `is_bdmv` (highest tier — full disc), else
    ///   ` Remux` if `is_remux` (lossless MKV extract), else no
    ///   suffix (encoded).
    pub fn label(&self) -> String {
        let source_label: String = match self.source {
            Source::Unknown => String::new(),
            Source::Web => match self.web_kind {
                WebKind::WebRip => "WEBRip".to_string(),
                // WebDl collapses to bare "WEB" at the label layer —
                // see the docstring above. The enum variant still
                // exists for CF matching and rank tiebreakers.
                WebKind::Unknown | WebKind::WebDl => "WEB".to_string(),
            },
            Source::BluRay => "BD".to_string(),
            other => other.as_str().to_string(),
        };

        // Base `SOURCE-RESOLUTION` (or just one of the two when the
        // other is missing). BluRay sub-tier gets appended after.
        let base = match (source_label.as_str(), self.resolution) {
            ("", Resolution::Unknown) => "Unknown".to_string(),
            (s, Resolution::Unknown) => s.to_string(),
            ("", r) => r.as_str().to_string(),
            (s, r) => format!("{}-{}", s, r.as_str()),
        };

        if matches!(self.source, Source::BluRay) {
            if self.is_bdmv {
                return format!("{} RAW", base);
            }
            if self.is_remux {
                return format!("{} Remux", base);
            }
        }

        base
    }
}

#[cfg(test)]
mod tests {
    //! Coverage for the pure source-type primitives. None of these
    //! touch the DB or the network — `from_str` / `as_str` /
    //! `rank()` / `label()` are all pure functions, but they're
    //! load-bearing: every classification result that ends up in
    //! `episode_quality_tags`, every CF spec evaluation, and every
    //! Sonarr-format quality string the user sees flows through these.
    //! A regression in any of them silently mis-tags every episode.
    //!
    //! The from_str round-trips also lock in the case-insensitive +
    //! variant-spelling acceptance that real-world tag strings use
    //! (`"WEB-DL"` vs `"webdl"` vs `"web.dl"`).
    use super::*;

    // ── Source ────────────────────────────────────────────────────────

    #[test]
    fn source_rank_orders_correctly() {
        // Strict total order; ties only between equal variants.
        assert!(Source::BluRay.rank() > Source::Web.rank());
        assert!(Source::Web.rank() > Source::Dvd.rank());
        assert!(Source::Dvd.rank() > Source::Hdtv.rank());
        assert!(Source::Hdtv.rank() > Source::Tv.rank());
        assert!(Source::Tv.rank() > Source::Unknown.rank());
    }

    #[test]
    fn source_from_str_accepts_canonical_and_variant_forms() {
        // Trash-Guides CFs and historical Ryokan rows both feed strings
        // through here, so the variant set has to cover what each emits.
        assert_eq!(Source::from_str("BluRay"), Source::BluRay);
        assert_eq!(Source::from_str("blu-ray"), Source::BluRay);
        assert_eq!(Source::from_str("BD"), Source::BluRay);
        assert_eq!(Source::from_str("bdrip"), Source::BluRay);
        assert_eq!(Source::from_str("BDRemux"), Source::BluRay);
        assert_eq!(Source::from_str("bdmv"), Source::BluRay);
        assert_eq!(Source::from_str("WEB"), Source::Web);
        assert_eq!(Source::from_str("Web-DL"), Source::Web);
        assert_eq!(Source::from_str("WebDl"), Source::Web);
        assert_eq!(Source::from_str("WEBRip"), Source::Web);
        assert_eq!(Source::from_str("DVD"), Source::Dvd);
        assert_eq!(Source::from_str("HDTV"), Source::Hdtv);
        assert_eq!(Source::from_str("TV"), Source::Tv);
    }

    #[test]
    fn source_from_str_unknown_for_garbage() {
        // Defensive default — a rogue tag string can't bypass the
        // classifier into a real source.
        assert_eq!(Source::from_str(""), Source::Unknown);
        assert_eq!(Source::from_str("not-a-source"), Source::Unknown);
        assert_eq!(Source::from_str("4K"), Source::Unknown); // resolution, not source
    }

    #[test]
    fn source_from_str_trims_whitespace() {
        // Real persisted tags occasionally carry trailing whitespace
        // from older write paths.
        assert_eq!(Source::from_str("  bluray  "), Source::BluRay);
        assert_eq!(Source::from_str("\tweb\n"), Source::Web);
    }

    // ── WebKind ───────────────────────────────────────────────────────

    #[test]
    fn web_kind_rank_webdl_beats_webrip_beats_unknown() {
        assert!(WebKind::WebDl.rank() > WebKind::WebRip.rank());
        assert!(WebKind::WebRip.rank() > WebKind::Unknown.rank());
    }

    #[test]
    fn web_kind_from_str_accepts_canonical_and_punctuated() {
        // Real tags carry every separator: `WEB-DL`, `WEBDL`, `WEB.DL`.
        assert_eq!(WebKind::from_str("WEB-DL"), WebKind::WebDl);
        assert_eq!(WebKind::from_str("webdl"), WebKind::WebDl);
        assert_eq!(WebKind::from_str("web.dl"), WebKind::WebDl);
        assert_eq!(WebKind::from_str("WEBRip"), WebKind::WebRip);
        assert_eq!(WebKind::from_str("web-rip"), WebKind::WebRip);
        assert_eq!(WebKind::from_str("web.rip"), WebKind::WebRip);
    }

    #[test]
    fn web_kind_unknown_renders_as_empty_string() {
        // The empty string is the contract — non-empty would pollute
        // `BD-1080p` / `WEB-1080p` style labels with a stray suffix
        // for the bare-WEB case the WebKind::Unknown variant exists for.
        assert_eq!(WebKind::Unknown.as_str(), "");
        assert_eq!(WebKind::WebDl.as_str(), "WEB-DL");
        assert_eq!(WebKind::WebRip.as_str(), "WEBRip");
    }

    // ── Resolution ────────────────────────────────────────────────────

    #[test]
    fn resolution_from_str_accepts_bare_and_suffixed() {
        // Settings-page emits `"1080"`; episode tags emit `"1080p"`.
        // Both paths flow through here.
        assert_eq!(Resolution::from_str("1080"), Resolution::R1080p);
        assert_eq!(Resolution::from_str("1080p"), Resolution::R1080p);
        assert_eq!(Resolution::from_str("1080P"), Resolution::R1080p);
        assert_eq!(Resolution::from_str("1080i"), Resolution::R1080p);
        assert_eq!(Resolution::from_str("720"), Resolution::R720p);
        assert_eq!(Resolution::from_str("720p"), Resolution::R720p);
        assert_eq!(Resolution::from_str("480"), Resolution::R480p);
        assert_eq!(Resolution::from_str("576p"), Resolution::R576p);
    }

    #[test]
    fn resolution_from_str_accepts_4k_aliases() {
        // `4k` / `UHD` are user-facing aliases for 2160p.
        for alias in ["2160", "2160p", "4k", "4K", "uhd", "UHD"] {
            assert_eq!(
                Resolution::from_str(alias),
                Resolution::R2160p,
                "alias {alias} should map to 2160p"
            );
        }
    }

    #[test]
    fn resolution_from_str_unknown_for_garbage() {
        assert_eq!(Resolution::from_str(""), Resolution::Unknown);
        assert_eq!(Resolution::from_str("not-a-res"), Resolution::Unknown);
        // Sonarr's 360 / 540 don't get tier'd by Ryokan.
        assert_eq!(Resolution::from_str("360p"), Resolution::Unknown);
        assert_eq!(Resolution::from_str("540p"), Resolution::Unknown);
    }

    #[test]
    fn resolution_from_dimensions_loose_lower_bounds() {
        // Lower-bound thresholds absorb anamorphic + cropped variants.
        // Pin the exact bands documented on `from_dimensions`.
        assert_eq!(Resolution::from_dimensions(3840, 2160), Resolution::R2160p);
        assert_eq!(Resolution::from_dimensions(1920, 1080), Resolution::R1080p);
        assert_eq!(Resolution::from_dimensions(1280, 720), Resolution::R720p);
        // Anamorphic 720p (704×480 squished from 720×480) lands in the
        // 480 band.
        assert_eq!(Resolution::from_dimensions(704, 480), Resolution::R480p);
        // PAL DVD (720×576).
        assert_eq!(Resolution::from_dimensions(720, 576), Resolution::R576p);
        // Below the floor → Unknown.
        assert_eq!(Resolution::from_dimensions(640, 360), Resolution::Unknown);
        // Boundary case: exactly the threshold.
        assert_eq!(Resolution::from_dimensions(0, 460), Resolution::R480p);
        assert_eq!(Resolution::from_dimensions(0, 459), Resolution::Unknown);
    }

    // ── ClassificationResult::label ──────────────────────────────────
    //
    // The label string is what `episode_quality_tags.quality_tag`
    // stores AND what the UI renders. A regression here mis-tags
    // every episode for the affected source class.

    fn cr(source: Source, res: Resolution, is_remux: bool, is_bdmv: bool) -> ClassificationResult {
        ClassificationResult {
            source,
            resolution: res,
            is_remux,
            web_kind: WebKind::Unknown,
            is_bdmv,
            confidence: 1.0,
            needs_review: false,
            evidence: Vec::new(),
            decision_rule: DecisionRule::Empty,
        }
    }

    #[test]
    fn label_canonical_sonarr_shapes() {
        // The four shapes the user sees most often. These strings are
        // the contract Sonarr-compat code matches against.
        assert_eq!(
            cr(Source::BluRay, Resolution::R1080p, false, false).label(),
            "BD-1080p"
        );
        assert_eq!(
            cr(Source::BluRay, Resolution::R1080p, true, false).label(),
            "BD-1080p Remux"
        );
        assert_eq!(
            cr(Source::BluRay, Resolution::R1080p, false, true).label(),
            "BD-1080p RAW"
        );
        assert_eq!(
            cr(Source::Web, Resolution::R1080p, false, false).label(),
            "WEB-1080p"
        );
        assert_eq!(
            cr(Source::Hdtv, Resolution::R1080p, false, false).label(),
            "HDTV-1080p"
        );
        assert_eq!(
            cr(Source::Dvd, Resolution::R480p, false, false).label(),
            "DVD-480p"
        );
    }

    #[test]
    fn label_collapses_webdl_to_bare_web() {
        // Issue #48: WebDl renders as bare WEB so users don't see two
        // labels for what's effectively the same tier. WebRip stays
        // distinct because that's the lower-quality variant power
        // users want to spot.
        let mut webdl = cr(Source::Web, Resolution::R1080p, false, false);
        webdl.web_kind = WebKind::WebDl;
        assert_eq!(webdl.label(), "WEB-1080p");

        let mut webrip = cr(Source::Web, Resolution::R1080p, false, false);
        webrip.web_kind = WebKind::WebRip;
        assert_eq!(webrip.label(), "WEBRip-1080p");
    }

    #[test]
    fn label_bdmv_takes_precedence_over_remux_when_both_set() {
        // Defensive: classifier shouldn't set both, but if it does,
        // BDMV wins per `bluray_tier()` — `is_bdmv` checks first.
        let cr = cr(Source::BluRay, Resolution::R1080p, true, true);
        assert_eq!(cr.label(), "BD-1080p RAW");
    }

    #[test]
    fn label_unknown_source_falls_back_to_resolution_only() {
        // No source determined but resolution available — show the
        // resolution alone rather than emit "Unknown-1080p" or similar.
        assert_eq!(
            cr(Source::Unknown, Resolution::R1080p, false, false).label(),
            "1080p"
        );
    }

    #[test]
    fn label_all_unknown_is_unknown() {
        assert_eq!(
            cr(Source::Unknown, Resolution::Unknown, false, false).label(),
            "Unknown"
        );
    }

    #[test]
    fn label_known_source_unknown_resolution_drops_resolution_suffix() {
        // BD-Unknown isn't useful; just `BD` is what the UI wants.
        assert_eq!(
            cr(Source::BluRay, Resolution::Unknown, false, false).label(),
            "BD"
        );
        // Sub-tier still lands.
        assert_eq!(
            cr(Source::BluRay, Resolution::Unknown, true, false).label(),
            "BD Remux"
        );
    }

    // ── ClassificationResult::rank ───────────────────────────────────

    #[test]
    fn rank_resolution_dominates_over_source() {
        // Web-1080p > BluRay-720p (resolution dominates the tuple).
        let web = cr(Source::Web, Resolution::R1080p, false, false);
        let bd = cr(Source::BluRay, Resolution::R720p, false, false);
        assert!(web.rank() > bd.rank());
    }

    #[test]
    fn rank_bluray_tier_breaks_ties() {
        // Same source + resolution, BDMV > Remux > plain.
        let plain = cr(Source::BluRay, Resolution::R1080p, false, false);
        let remux = cr(Source::BluRay, Resolution::R1080p, true, false);
        let bdmv = cr(Source::BluRay, Resolution::R1080p, false, true);
        assert!(remux.rank() > plain.rank());
        assert!(bdmv.rank() > remux.rank());
    }

    #[test]
    fn rank_web_kind_breaks_ties() {
        // Pins the actual rank order: WebDl (2) > WebRip (1) >
        // Unknown (0). The `WebKind` docstring claims bare-WEB "slots
        // between WebRip and WebDl," but the code sits Unknown at the
        // bottom — explicit WebRip outranks an untagged Web release.
        // A consequence is that a `WEBRip` row on disk never gets
        // upgraded by a same-resolution bare-WEB release (the upgrade
        // gate compares `incoming.rank > existing.rank`); whether
        // that's the intended semantics is a separate decision, but
        // the test pins the current behavior so a refactor that
        // touches the rank order has to confront the doc/code
        // mismatch.
        let mut webdl = cr(Source::Web, Resolution::R1080p, false, false);
        webdl.web_kind = WebKind::WebDl;
        let mut webrip = cr(Source::Web, Resolution::R1080p, false, false);
        webrip.web_kind = WebKind::WebRip;
        let webunknown = cr(Source::Web, Resolution::R1080p, false, false);
        assert!(webdl.rank() > webrip.rank());
        assert!(webrip.rank() > webunknown.rank());
    }

    // ── SourceEvidence::new clamps confidence to 0..=1 ───────────────

    #[test]
    fn source_evidence_new_clamps_confidence() {
        // Layer authors emit confidences from heterogeneous sources;
        // clamp at the boundary so a buggy 1.5 doesn't dominate
        // aggregation, and a -0.1 doesn't subtract from the running sum.
        assert_eq!(
            SourceEvidence::new(Source::BluRay, 1.5, Origin::Filename, "").confidence,
            1.0
        );
        assert_eq!(
            SourceEvidence::new(Source::BluRay, -0.1, Origin::Filename, "").confidence,
            0.0
        );
        assert_eq!(
            SourceEvidence::new(Source::BluRay, 0.5, Origin::Filename, "").confidence,
            0.5
        );
    }
}
