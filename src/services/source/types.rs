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
    /// Used by the Custom Formats parser to compile a
    /// `ResolutionSpecification` into a comparison against
    /// `ClassificationResult::resolution`. Phase 3 lands the helper in
    /// isolation; Phase 4 (`src/services/custom_formats.rs`) adds the
    /// caller, at which point the `#[allow(dead_code)]` comes off.
    #[allow(dead_code)]
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
    /// `BD-1080p`, `BD-1080p Remux`, `BD-1080p RAW`, `WEBDL-1080p`,
    /// `WEBRip-1080p`, `WEB-1080p`, `HDTV-1080p`, `DVD-480p`, etc.
    ///
    /// Keeping the resolution adjacent to the source (rather than
    /// appending it after the sub-tier) preserves parity with Sonarr's
    /// quality definition names, which treat `<source>-<resolution>`
    /// as the immutable key and bolt any Remux qualifier on afterward.
    ///
    /// Source rendering is variant-aware:
    /// - `Source::Web` with a known `web_kind` displays as "WEBDL" or
    ///   "WEBRip" (Sonarr-style, no internal hyphen) instead of the
    ///   bare "WEB" fallback.
    /// - `Source::BluRay` always renders as `BD` with at most one
    ///   trailing space-separated qualifier, mutually exclusive:
    ///   ` RAW` if `is_bdmv` (highest tier — full disc), else
    ///   ` Remux` if `is_remux` (lossless MKV extract), else no
    ///   suffix (encoded).
    pub fn label(&self) -> String {
        let source_label: String = match self.source {
            Source::Unknown => String::new(),
            Source::Web => match self.web_kind {
                WebKind::WebDl => "WEBDL".to_string(),
                WebKind::WebRip => "WEBRip".to_string(),
                WebKind::Unknown => "WEB".to_string(),
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
