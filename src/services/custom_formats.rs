//! Sonarr-v4-compatible Custom Formats.
//!
//! A Custom Format is a user-authored bundle of specifications that
//! either matches a release (CF score adds to the candidate's total)
//! or doesn't. Ryokan's CF model is a strict subset of Sonarr v4's —
//! same JSON shape, same match semantics — with a single Ryokan-only
//! addition (`Ryokan.SeaDexBestSpecification`) surfaced via the
//! `Ryokan.` namespace so Sonarr-safe exports can detect and skip it.
//!
//! This module owns the parser, the per-candidate evaluator, and the
//! DB-backed startup loader. It does **not** own cache invalidation or
//! the `AppState` plumbing — Phase 5 wires the compiled-CF cache onto
//! `AppState` and adds `rebuild_cf_cache`, and Phase 6 plugs
//! `total_cf_score` into the three `auto_search.rs` call sites.
//!
//! The critical piece of correctness here is §5.7 of the plan: the
//! match rule is NOT "all specs true" — it's group-by-type DidMatch
//! with a subtle required-hard-fail rule. See [`evaluate`] and the
//! worked examples in its unit tests for the exact semantics.

// Phase 4 lands the module in isolation; Phases 5–6 wire the caller
// (AppState, startup load, auto_search integration). Until those
// phases land, `load_compiled_cfs` and `total_cf_score` are referenced
// only by unit tests, which trips `-D warnings` dead-code errors in
// clippy. Scope the allow to this file so the warnings come back the
// moment a caller goes missing later.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::RwLock;

use super::nyaa::SearchResult;
use super::source::{ClassificationResult, Resolution, Source, WebKind};

// ───────────────────────────────────────────────────────────────────────────
// Types
// ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CompiledCustomFormat {
    pub id: i64,
    pub name: String,
    pub score: i32,
    pub specs: Vec<CompiledSpec>,
}

/// Every spec variant carries both `negate` and `required`, matching
/// Sonarr's `ICustomFormatSpecification` interface. `required` is
/// consumed by the group-by-type DidMatch rule in [`evaluate`] (§5.7),
/// not inside [`evaluate_spec`].
#[derive(Debug, Clone)]
pub struct CompiledSpec {
    pub kind: SpecKind,
    pub negate: bool,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub enum SpecKind {
    ReleaseTitle { regex: regex_lite::Regex },
    ReleaseGroup { regex: regex_lite::Regex },
    Size { min_bytes: i64, max_bytes: i64 },
    Resolution { value: Resolution },
    /// Stores Sonarr's raw `QualitySource` integer (see plan §4.5).
    /// Dispatch happens inside `evaluate_spec_kernel` — one branch per
    /// supported Sonarr int, with `2` (TelevisionRaw) rejected at parse
    /// time.
    Source { sonarr_value: u8 },
    /// Ryokan-only: matches when the candidate's info_hash is in the
    /// SeaDex "best" hash set for the current anilist_id. Namespaced
    /// as `Ryokan.SeaDexBestSpecification` in exported JSON.
    SeaDexBest,
}

impl SpecKind {
    /// Group-by-type discriminator used by [`evaluate`]. Two specs
    /// belong to the same group iff this returns the same value.
    /// Mirrors Sonarr's `.GroupBy(t => t.GetType())`.
    pub fn type_tag(&self) -> u8 {
        match self {
            SpecKind::ReleaseTitle { .. } => 1,
            SpecKind::ReleaseGroup { .. } => 2,
            SpecKind::Size { .. } => 3,
            SpecKind::Resolution { .. } => 4,
            SpecKind::Source { .. } => 5,
            SpecKind::SeaDexBest => 6,
        }
    }
}

/// Evaluation context threaded through [`evaluate`] / [`total_cf_score`].
///
/// Holds borrowed references to the candidate, its classification, and
/// the (possibly empty) set of SeaDex best hashes for the current
/// anilist_id. Lifetimes keep everything non-allocating on the per-
/// candidate path.
pub struct EvalContext<'a> {
    pub result: &'a SearchResult,
    pub classification: &'a ClassificationResult,
    /// Lowercased info hashes that SeaDex has flagged as `isBest` for
    /// the current anilist_id. Empty when SeaDex is disabled, the entry
    /// is missing, or `pick_best` rejected every candidate.
    pub seadex_hashes: &'a HashSet<String>,
}

/// Shared cache container — an `RwLock` around an `Arc<Vec<...>>` so
/// evaluation code can cheap-clone the inner `Arc` and release the read
/// lock before iterating over candidates. Phase 5 adds a field of this
/// type to `AppState`; this alias is declared here so both sides agree
/// on the shape.
pub type CompiledCfCache = Arc<RwLock<Arc<Vec<CompiledCustomFormat>>>>;

// ───────────────────────────────────────────────────────────────────────────
// Parser
// ───────────────────────────────────────────────────────────────────────────

/// Compile a single Sonarr-shape CF JSON blob into an evaluator-ready
/// [`CompiledCustomFormat`].
///
/// Rejects, with an explanatory error, any CF whose intent cannot be
/// faithfully represented in Ryokan:
/// - All of the original specs were of unsupported types (e.g. a
///   TRaSH `BR-DISK` CF with only a `QualityModifierSpecification`).
///   Silently collapsing would leave `specs=[]`, which under the §5.7.2
///   vacuous-truth rule would match every release.
/// - A `required=true` spec was dropped for being unsupported. Dropping
///   a required spec would change the DidMatch gate for its group.
///
/// All regex compilation happens here once, not per-candidate: a 1000-
/// candidate search over 10 CFs with one regex each would otherwise
/// compile 10k regexes.
pub fn compile_from_json(raw: &str, score: i32, id: i64) -> Result<CompiledCustomFormat, String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("CF JSON parse: {e}"))?;

    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| "CF missing name".to_string())?
        .to_string();

    let specs_json = v
        .get("specifications")
        .and_then(|s| s.as_array())
        .ok_or_else(|| "CF missing specifications".to_string())?;

    let original_spec_count = specs_json.len();
    let mut specs: Vec<CompiledSpec> = Vec::new();
    let mut dropped_required: Option<String> = None;

    for spec_v in specs_json {
        let implementation = spec_v
            .get("implementation")
            .and_then(|i| i.as_str())
            .unwrap_or("");
        let negate = spec_v.get("negate").and_then(|b| b.as_bool()).unwrap_or(false);
        let required = spec_v.get("required").and_then(|b| b.as_bool()).unwrap_or(false);
        let empty_fields: Vec<serde_json::Value> = Vec::new();
        let fields = spec_v
            .get("fields")
            .and_then(|f| f.as_array())
            .unwrap_or(&empty_fields);

        let kind: SpecKind = match implementation {
            "ReleaseTitleSpecification" => {
                let value = field_str(fields, "value")?;
                // Sonarr regexes are case-insensitive by convention;
                // prepend `(?i)` so every imported CF matches regardless
                // of how the user authored it.
                let re = regex_lite::Regex::new(&format!("(?i){value}"))
                    .map_err(|e| format!("ReleaseTitle regex: {e}"))?;
                SpecKind::ReleaseTitle { regex: re }
            }
            "ReleaseGroupSpecification" => {
                let value = field_str(fields, "value")?;
                let re = regex_lite::Regex::new(&format!("(?i){value}"))
                    .map_err(|e| format!("ReleaseGroup regex: {e}"))?;
                SpecKind::ReleaseGroup { regex: re }
            }
            "SizeSpecification" => {
                // Sonarr ships size bounds as GB floats. Field names
                // are literally `min` and `max` (verified against
                // Sonarr's SizeSpecification.cs).
                let min = field_f64(fields, "min").unwrap_or(0.0);
                let max = field_f64(fields, "max").unwrap_or(f64::INFINITY);
                const GB: f64 = 1024.0 * 1024.0 * 1024.0;
                SpecKind::Size {
                    min_bytes: (min * GB) as i64,
                    max_bytes: if max.is_finite() {
                        (max * GB) as i64
                    } else {
                        i64::MAX
                    },
                }
            }
            "ResolutionSpecification" => {
                let value = field_i64(fields, "value")? as i32;
                SpecKind::Resolution {
                    value: Resolution::from_int(value),
                }
            }
            "SourceSpecification" => {
                let value = field_i64(fields, "value")?;
                // Reject TelevisionRaw (2) — no anime mapping. Also
                // reject anything out of range. Treat like an unknown
                // implementation so the drop/required logic below fires.
                if value == 2 || !(0..=7).contains(&value) {
                    tracing::warn!(
                        "custom_formats: skipping SourceSpecification(value={value}) on CF `{name}`"
                    );
                    if required && dropped_required.is_none() {
                        dropped_required = Some(format!("SourceSpecification(value={value})"));
                    }
                    continue;
                }
                SpecKind::Source {
                    sonarr_value: value as u8,
                }
            }
            // Ryokan-only spec: accept both the namespaced form (new
            // default, see plan §5.7.5) and the bare form for backwards
            // compatibility with older Ryokan exports.
            "Ryokan.SeaDexBestSpecification" | "SeaDexBestSpecification" => SpecKind::SeaDexBest,
            other => {
                tracing::warn!(
                    "custom_formats: skipping unsupported spec `{other}` on CF `{name}`"
                );
                if required && dropped_required.is_none() {
                    dropped_required = Some(other.to_string());
                }
                continue;
            }
        };

        specs.push(CompiledSpec { kind, negate, required });
    }

    // Cross-compat safety: a CF whose intent cannot be preserved must
    // not silently become a match-everything vacuous-truth CF. See
    // plan §5.7.2 for why.
    if original_spec_count > 0 && specs.is_empty() {
        return Err(format!(
            "all {original_spec_count} specifications unsupported — CF `{name}` cannot be \
             represented in Ryokan and was rejected to avoid a vacuous-match CF"
        ));
    }
    if let Some(label) = dropped_required {
        return Err(format!(
            "required=true specification `{label}` was dropped — CF `{name}` semantics \
             cannot be preserved and the CF was rejected"
        ));
    }

    Ok(CompiledCustomFormat { id, name, score, specs })
}

fn field_str(fields: &[serde_json::Value], name: &str) -> Result<String, String> {
    for f in fields {
        if f.get("name").and_then(|n| n.as_str()) == Some(name) {
            return Ok(f
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string());
        }
    }
    Err(format!("field `{name}` missing"))
}

fn field_i64(fields: &[serde_json::Value], name: &str) -> Result<i64, String> {
    for f in fields {
        if f.get("name").and_then(|n| n.as_str()) == Some(name) {
            return f
                .get("value")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| format!("field `{name}` not an integer"));
        }
    }
    Err(format!("field `{name}` missing"))
}

fn field_f64(fields: &[serde_json::Value], name: &str) -> Option<f64> {
    for f in fields {
        if f.get("name").and_then(|n| n.as_str()) == Some(name) {
            // Sonarr ships these as JSON numbers; accept either f64 or
            // i64 encodings for robustness (`"min": 5` vs `"min": 5.0`).
            return f
                .get("value")
                .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|n| n as f64)));
        }
    }
    None
}

// ───────────────────────────────────────────────────────────────────────────
// Per-spec kernel + negate wrapper
// ───────────────────────────────────────────────────────────────────────────

/// Raw per-spec match, pre-negate. Mirrors Sonarr's
/// `IsSatisfiedByWithoutNegate`.
fn evaluate_spec_kernel(spec: &CompiledSpec, ctx: &EvalContext) -> bool {
    match &spec.kind {
        SpecKind::ReleaseTitle { regex } => regex.is_match(&ctx.result.title),
        // `SearchResult::group` is a bare String. Empty means the Nyaa
        // scraper didn't find a `[Group]` prefix; an empty-string regex
        // still matches it, which is consistent with Sonarr's behavior.
        SpecKind::ReleaseGroup { regex } => regex.is_match(&ctx.result.group),
        SpecKind::Size { min_bytes, max_bytes } => {
            // Sonarr's SizeSpecification.cs uses strict-greater on the
            // lower bound and ≤ on the upper bound: `size > Min &&
            // size <= Max`. Mirror exactly.
            let s = ctx.result.size_bytes;
            s > *min_bytes && s <= *max_bytes
        }
        SpecKind::Resolution { value } => ctx.classification.resolution == *value,
        SpecKind::Source { sonarr_value } => {
            let c = ctx.classification;
            match sonarr_value {
                0 => c.source == Source::Unknown,
                1 => matches!(c.source, Source::Hdtv | Source::Tv),
                3 => c.source == Source::Web && c.web_kind == WebKind::WebDl,
                4 => c.source == Source::Web && c.web_kind == WebKind::WebRip,
                5 => c.source == Source::Dvd,
                6 => c.source == Source::BluRay && !c.is_bdmv,
                7 => c.source == Source::BluRay && c.is_bdmv,
                // 2 (TelevisionRaw) and out-of-range are filtered at
                // parse time, so they never reach here.
                _ => false,
            }
        }
        SpecKind::SeaDexBest => {
            !ctx.result.info_hash.is_empty()
                && ctx
                    .seadex_hashes
                    .contains(&ctx.result.info_hash.to_ascii_lowercase())
        }
    }
}

/// Per-spec match with Sonarr's `Negate` applied. This is the input to
/// the group-by-type DidMatch rule in [`evaluate`] — never call the
/// kernel directly from there.
fn evaluate_spec(spec: &CompiledSpec, ctx: &EvalContext) -> bool {
    let raw = evaluate_spec_kernel(spec, ctx);
    if spec.negate {
        !raw
    } else {
        raw
    }
}

// ───────────────────────────────────────────────────────────────────────────
// CF-level match + score summation
// ───────────────────────────────────────────────────────────────────────────

/// Does this CF match the candidate?
///
/// Implements Sonarr v4's group-by-type DidMatch rule verbatim. See
/// plan §5.7.1 for the Sonarr source snippet this mirrors, and §5.7.3
/// for worked examples.
///
/// The rule is NOT "all specs true" and NOT "any spec true". It's:
/// 1. Group specs by `type_tag()`.
/// 2. Within each group: match iff no `required=true` spec returned
///    false AND at least one spec returned true.
/// 3. CF matches iff every group matches.
pub fn evaluate(cf: &CompiledCustomFormat, ctx: &EvalContext) -> bool {
    // Vacuous-truth parity with Sonarr: a CF with zero specs produces
    // an empty groups list, and `empty.All(x => x.DidMatch)` is `true`
    // in LINQ (same as `.all()` in Rust). Real imports can't reach this
    // branch — `compile_from_json` rejects all-unsupported CFs — but
    // strict parity means we mirror Sonarr rather than second-guessing.
    if cf.specs.is_empty() {
        return true;
    }

    let mut groups: BTreeMap<u8, Vec<(&CompiledSpec, bool)>> = BTreeMap::new();
    for spec in &cf.specs {
        let matched = evaluate_spec(spec, ctx);
        groups.entry(spec.kind.type_tag()).or_default().push((spec, matched));
    }

    groups.values().all(|group| {
        let any_required_failed = group.iter().any(|(s, m)| s.required && !m);
        let all_failed = group.iter().all(|(_, m)| !m);
        !(any_required_failed || all_failed)
    })
}

/// Sum the scores of every CF that matches the candidate. Non-matching
/// CFs contribute 0 regardless of their score sign. Used by the Phase 6
/// auto_search integration as a single-call overlay on `base_score`.
pub fn total_cf_score(cfs: &[CompiledCustomFormat], ctx: &EvalContext) -> i32 {
    cfs.iter()
        .filter(|cf| evaluate(cf, ctx))
        .map(|cf| cf.score)
        .sum()
}

// ───────────────────────────────────────────────────────────────────────────
// DB-backed startup loader
// ───────────────────────────────────────────────────────────────────────────

/// Load every CF row, join its V1-profile score, and compile each one.
/// A CF that fails to parse is logged at WARN and skipped; the rest of
/// the set still loads. Phase 5 wraps the returned Vec in an `Arc` and
/// stashes it on `AppState` at startup.
pub async fn load_compiled_cfs(db: &SqlitePool) -> Vec<CompiledCustomFormat> {
    let rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
        r#"
        SELECT cf.id, cf.name, cf.json, COALESCE(cfs.score, 0) AS score
        FROM custom_formats cf
        LEFT JOIN custom_format_scores cfs
               ON cfs.custom_format_id = cf.id
              AND cfs.profile_id = 1
        ORDER BY cf.id
        "#,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .filter_map(|(id, name, raw_json, score)| {
            match compile_from_json(&raw_json, score as i32, id) {
                Ok(cf) => Some(cf),
                Err(e) => {
                    tracing::warn!("custom_formats: skipping {name} (id={id}): {e}");
                    None
                }
            }
        })
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::source::{DecisionRule, Source};

    // ── Helpers ──────────────────────────────────────────────────────────

    fn candidate(title: &str, group: &str, size_bytes: i64, info_hash: &str) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            link: String::new(),
            magnet: String::new(),
            torrent: String::new(),
            size: String::new(),
            size_bytes,
            seeders: 0,
            leechers: 0,
            downloads: 0,
            group: group.to_string(),
            resolution: String::new(),
            is_batch: false,
            is_trusted: false,
            score: 0,
            info_hash: info_hash.to_string(),
        }
    }

    fn classification(source: Source, resolution: Resolution) -> ClassificationResult {
        ClassificationResult {
            source,
            resolution,
            is_remux: false,
            web_kind: WebKind::Unknown,
            is_bdmv: false,
            confidence: 1.0,
            needs_review: false,
            evidence: vec![],
            decision_rule: DecisionRule::Empty,
        }
    }

    fn ctx<'a>(
        result: &'a SearchResult,
        classification: &'a ClassificationResult,
        seadex: &'a HashSet<String>,
    ) -> EvalContext<'a> {
        EvalContext {
            result,
            classification,
            seadex_hashes: seadex,
        }
    }

    fn compile(raw: &str) -> CompiledCustomFormat {
        compile_from_json(raw, 100, 1).expect("fixture CF should compile")
    }

    // ── Parser: basic shapes and field extraction ────────────────────────

    #[test]
    fn parse_release_title_spec() {
        let json = r#"{
            "name": "x265 preference",
            "specifications": [
                {
                    "name": "x265",
                    "implementation": "ReleaseTitleSpecification",
                    "implementationName": "Release Title",
                    "negate": false,
                    "required": false,
                    "fields": [{"name": "value", "value": "x265"}]
                }
            ]
        }"#;
        let cf = compile(json);
        assert_eq!(cf.name, "x265 preference");
        assert_eq!(cf.specs.len(), 1);
        assert!(matches!(cf.specs[0].kind, SpecKind::ReleaseTitle { .. }));
    }

    #[test]
    fn parse_size_spec_converts_gb_to_bytes() {
        let json = r#"{
            "name": "5-20 GB",
            "specifications": [
                {
                    "implementation": "SizeSpecification",
                    "fields": [
                        {"name": "min", "value": 5},
                        {"name": "max", "value": 20}
                    ]
                }
            ]
        }"#;
        let cf = compile(json);
        if let SpecKind::Size { min_bytes, max_bytes } = cf.specs[0].kind {
            const GB: i64 = 1024 * 1024 * 1024;
            assert_eq!(min_bytes, 5 * GB);
            assert_eq!(max_bytes, 20 * GB);
        } else {
            panic!("expected Size spec");
        }
    }

    #[test]
    fn parse_resolution_spec_uses_from_int() {
        let json = r#"{
            "name": "1080p",
            "specifications": [
                {
                    "implementation": "ResolutionSpecification",
                    "fields": [{"name": "value", "value": 1080}]
                }
            ]
        }"#;
        let cf = compile(json);
        assert!(matches!(
            cf.specs[0].kind,
            SpecKind::Resolution { value: Resolution::R1080p }
        ));
    }

    #[test]
    fn parse_source_spec_webdl() {
        let json = r#"{
            "name": "WEB-DL",
            "specifications": [
                {
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 3}]
                }
            ]
        }"#;
        let cf = compile(json);
        assert!(matches!(
            cf.specs[0].kind,
            SpecKind::Source { sonarr_value: 3 }
        ));
    }

    #[test]
    fn parse_source_spec_rejects_television_raw() {
        // Sonarr's TelevisionRaw (2) has no anime mapping, so the
        // parser drops it. In this case it's the only spec and its
        // removal would leave a vacuous-match CF, so compilation fails.
        let json = r#"{
            "name": "tv raw only",
            "specifications": [
                {
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 2}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 0, 1).unwrap_err();
        assert!(err.contains("all 1 specifications unsupported"));
    }

    #[test]
    fn parse_source_spec_rejects_out_of_range() {
        let json = r#"{
            "name": "bogus",
            "specifications": [
                {
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 42}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 0, 1).unwrap_err();
        assert!(err.contains("all 1 specifications unsupported"));
    }

    #[test]
    fn parse_seadex_best_namespaced_and_bare() {
        let namespaced = r#"{
            "name": "seadex new",
            "specifications": [
                {
                    "implementation": "Ryokan.SeaDexBestSpecification",
                    "fields": []
                }
            ]
        }"#;
        let bare = r#"{
            "name": "seadex old",
            "specifications": [
                {
                    "implementation": "SeaDexBestSpecification",
                    "fields": []
                }
            ]
        }"#;
        assert!(matches!(compile(namespaced).specs[0].kind, SpecKind::SeaDexBest));
        assert!(matches!(compile(bare).specs[0].kind, SpecKind::SeaDexBest));
    }

    #[test]
    fn parse_rejects_all_unsupported() {
        // Simulates TRaSH's `BR-DISK` CF: only a spec type Ryokan
        // doesn't implement. Must NOT collapse to an empty specs vec.
        let json = r#"{
            "name": "BR-DISK",
            "specifications": [
                {
                    "implementation": "QualityModifierSpecification",
                    "fields": [{"name": "value", "value": 6}]
                }
            ]
        }"#;
        let err = compile_from_json(json, -10000, 1).unwrap_err();
        assert!(
            err.contains("all 1 specifications unsupported"),
            "error should mention vacuous-match guard: {err}"
        );
    }

    #[test]
    fn parse_rejects_dropped_required_spec() {
        // A CF with a surviving ReleaseTitle spec but a required=true
        // LanguageSpecification (unsupported) must be rejected, because
        // dropping the required spec would change the DidMatch gate.
        let json = r#"{
            "name": "english x265",
            "specifications": [
                {
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                },
                {
                    "implementation": "LanguageSpecification",
                    "required": true,
                    "fields": [{"name": "value", "value": 1}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 0, 1).unwrap_err();
        assert!(err.contains("LanguageSpecification"), "got: {err}");
        assert!(err.contains("required=true"), "got: {err}");
    }

    #[test]
    fn parse_rejects_invalid_regex() {
        let json = r#"{
            "name": "bad regex",
            "specifications": [
                {
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "[unclosed"}]
                }
            ]
        }"#;
        assert!(compile_from_json(json, 0, 1).is_err());
    }

    #[test]
    fn parse_rejects_missing_name() {
        let json = r#"{"specifications": []}"#;
        assert!(compile_from_json(json, 0, 1).is_err());
    }

    #[test]
    fn parse_rejects_missing_specifications_key() {
        let json = r#"{"name": "lonely"}"#;
        assert!(compile_from_json(json, 0, 1).is_err());
    }

    // ── evaluate_spec_kernel / evaluate_spec ─────────────────────────────

    #[test]
    fn release_title_kernel_matches_case_insensitive() {
        let cf = compile(
            r#"{
                "name": "x265",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();

        let hit = candidate("[MTBB] Show - 01 [BD 1080p X265]", "MTBB", 0, "");
        let cls = classification(Source::BluRay, Resolution::R1080p);
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));

        let miss = candidate("[Judas] Show - 01 [1080p]", "Judas", 0, "");
        assert!(!evaluate(&cf, &ctx(&miss, &cls, &hashes)));
    }

    #[test]
    fn release_group_spec_matches_parsed_group_not_title() {
        // The plan's "win over Sonarr": `^MTBB$` anchored against the
        // parsed group field, not the whole title. Sonarr's older
        // behavior would fail to match because the title carries
        // surrounding characters.
        let cf = compile(
            r#"{
                "name": "MTBB only",
                "specifications": [{
                    "implementation": "ReleaseGroupSpecification",
                    "fields": [{"name": "value", "value": "^MTBB$"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::BluRay, Resolution::R1080p);

        let hit = candidate("[MTBB] Kizumonogatari - 01 (BD 1080p)", "MTBB", 0, "");
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));

        let miss = candidate("[NoobSubs] Kizumonogatari - 01", "NoobSubs", 0, "");
        assert!(!evaluate(&cf, &ctx(&miss, &cls, &hashes)));
    }

    #[test]
    fn size_spec_strict_lower_inclusive_upper() {
        // Match Sonarr: `size > min && size <= max`.
        let cf = compile(
            r#"{
                "name": "5 to 20 GB",
                "specifications": [{
                    "implementation": "SizeSpecification",
                    "fields": [
                        {"name": "min", "value": 5},
                        {"name": "max", "value": 20}
                    ]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::BluRay, Resolution::R1080p);
        const GB: i64 = 1024 * 1024 * 1024;

        // Exactly at the lower bound: strict-greater means NO match.
        let at_min = candidate("x", "", 5 * GB, "");
        assert!(!evaluate(&cf, &ctx(&at_min, &cls, &hashes)));

        // Just above the lower bound: match.
        let just_above = candidate("x", "", 5 * GB + 1, "");
        assert!(evaluate(&cf, &ctx(&just_above, &cls, &hashes)));

        // Exactly at the upper bound: inclusive means match.
        let at_max = candidate("x", "", 20 * GB, "");
        assert!(evaluate(&cf, &ctx(&at_max, &cls, &hashes)));

        // Above the upper bound: no match.
        let above = candidate("x", "", 20 * GB + 1, "");
        assert!(!evaluate(&cf, &ctx(&above, &cls, &hashes)));
    }

    #[test]
    fn resolution_spec_uses_classification_not_filename() {
        // The whole point: filename-parsed `SearchResult::resolution`
        // string is ignored; we compare against the pipeline's
        // structured `ClassificationResult::resolution`.
        let cf = compile(
            r#"{
                "name": "1080p only",
                "specifications": [{
                    "implementation": "ResolutionSpecification",
                    "fields": [{"name": "value", "value": 1080}]
                }]
            }"#,
        );
        let hashes = HashSet::new();

        let cand = candidate("cosmetic 720p in filename", "", 0, "");
        // The candidate's filename says 720p but the classifier decided
        // R1080p — the CF trusts the classifier.
        let cls = classification(Source::Web, Resolution::R1080p);
        assert!(evaluate(&cf, &ctx(&cand, &cls, &hashes)));

        let cls_720 = classification(Source::Web, Resolution::R720p);
        assert!(!evaluate(&cf, &ctx(&cand, &cls_720, &hashes)));
    }

    #[test]
    fn source_spec_webdl_vs_webrip_vs_bare_web() {
        let webdl_cf = compile(
            r#"{
                "name": "WEB-DL",
                "specifications": [{
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 3}]
                }]
            }"#,
        );
        let webrip_cf = compile(
            r#"{
                "name": "WEBRip",
                "specifications": [{
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 4}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cand = candidate("x", "", 0, "");

        let mut webdl = classification(Source::Web, Resolution::R1080p);
        webdl.web_kind = WebKind::WebDl;
        let mut webrip = classification(Source::Web, Resolution::R1080p);
        webrip.web_kind = WebKind::WebRip;
        let bare = classification(Source::Web, Resolution::R1080p); // Unknown

        assert!(evaluate(&webdl_cf, &ctx(&cand, &webdl, &hashes)));
        assert!(!evaluate(&webdl_cf, &ctx(&cand, &webrip, &hashes)));
        assert!(!evaluate(&webdl_cf, &ctx(&cand, &bare, &hashes)));

        assert!(evaluate(&webrip_cf, &ctx(&cand, &webrip, &hashes)));
        assert!(!evaluate(&webrip_cf, &ctx(&cand, &webdl, &hashes)));
        // Bare-WEB deliberately matches neither (plan §4.5).
        assert!(!evaluate(&webrip_cf, &ctx(&cand, &bare, &hashes)));
    }

    #[test]
    fn source_spec_bluray_vs_bluray_raw() {
        let bluray_cf = compile(
            r#"{
                "name": "BluRay",
                "specifications": [{
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 6}]
                }]
            }"#,
        );
        let bluray_raw_cf = compile(
            r#"{
                "name": "BluRay RAW",
                "specifications": [{
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 7}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cand = candidate("x", "", 0, "");

        let encode = classification(Source::BluRay, Resolution::R1080p);
        let mut bdmv = classification(Source::BluRay, Resolution::R1080p);
        bdmv.is_bdmv = true;

        assert!(evaluate(&bluray_cf, &ctx(&cand, &encode, &hashes)));
        assert!(!evaluate(&bluray_cf, &ctx(&cand, &bdmv, &hashes))); // BDMV excluded by !is_bdmv

        assert!(evaluate(&bluray_raw_cf, &ctx(&cand, &bdmv, &hashes)));
        assert!(!evaluate(&bluray_raw_cf, &ctx(&cand, &encode, &hashes)));
    }

    #[test]
    fn seadex_best_matches_lowercased_hash_set() {
        let cf = compile(
            r#"{
                "name": "SeaDex",
                "specifications": [{
                    "implementation": "Ryokan.SeaDexBestSpecification",
                    "fields": []
                }]
            }"#,
        );
        let mut hashes = HashSet::new();
        hashes.insert("abc123".to_string());
        let cls = classification(Source::BluRay, Resolution::R1080p);

        // Exact match.
        let in_set = candidate("x", "", 0, "abc123");
        assert!(evaluate(&cf, &ctx(&in_set, &cls, &hashes)));

        // Uppercase match via lowercasing on compare.
        let in_set_upper = candidate("x", "", 0, "ABC123");
        assert!(evaluate(&cf, &ctx(&in_set_upper, &cls, &hashes)));

        // Miss.
        let not_in_set = candidate("x", "", 0, "def456");
        assert!(!evaluate(&cf, &ctx(&not_in_set, &cls, &hashes)));

        // Empty hash never matches.
        let no_hash = candidate("x", "", 0, "");
        assert!(!evaluate(&cf, &ctx(&no_hash, &cls, &hashes)));

        // Empty set never matches.
        let empty = HashSet::new();
        assert!(!evaluate(&cf, &ctx(&in_set, &cls, &empty)));
    }

    // ── Group-by-type DidMatch rule (§5.7.3 worked examples) ─────────────

    #[test]
    fn example_a_single_spec_match() {
        let cf = compile(
            r#"{
                "name": "A",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::BluRay, Resolution::R1080p);
        let hit = candidate("[MTBB] Show - 01 [BD 1080p x265]", "MTBB", 0, "");
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));
    }

    #[test]
    fn example_b_or_within_same_type() {
        // Two ReleaseTitle specs in the same group — OR within type.
        let cf = compile(
            r#"{
                "name": "B",
                "specifications": [
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "fields": [{"name": "value", "value": "x265"}]
                    },
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "fields": [{"name": "value", "value": "HEVC"}]
                    }
                ]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);
        // Only HEVC hits — the other spec in the same group is false,
        // but OR within group means the whole group still matches.
        let hit = candidate("[Judas] Show - 01 [HEVC]", "Judas", 0, "");
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));
    }

    #[test]
    fn example_c_and_across_groups() {
        // ReleaseTitle ∧ Size — different type_tags, groups must all
        // match.
        let cf = compile(
            r#"{
                "name": "C",
                "specifications": [
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "fields": [{"name": "value", "value": "x265"}]
                    },
                    {
                        "implementation": "SizeSpecification",
                        "fields": [
                            {"name": "min", "value": 5},
                            {"name": "max", "value": 20}
                        ]
                    }
                ]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::BluRay, Resolution::R1080p);
        const GB: i64 = 1024 * 1024 * 1024;

        // Both groups match.
        let hit = candidate("[MTBB] Show - 01 [BD 1080p x265]", "MTBB", 12 * GB, "");
        assert!(evaluate(&cf, &ctx(&hit, &cls, &hashes)));

        // Title group fails — CF fails even though size is in range.
        let title_miss = candidate("[SubsPlease] Show - 01 (1080p)", "SubsPlease", 6 * GB, "");
        assert!(!evaluate(&cf, &ctx(&title_miss, &cls, &hashes)));

        // Size group fails — CF fails even though title matches.
        let size_miss = candidate("[MTBB] Show - 01 [BD 1080p x265]", "MTBB", 1_200_000_000, "");
        assert!(!evaluate(&cf, &ctx(&size_miss, &cls, &hashes)));
    }

    #[test]
    fn example_d_required_hard_gate_within_group() {
        // Two ReleaseTitle specs in the same group, one with required=true.
        // When the required spec fails, the whole group fails even though
        // the OR partner matched — required=true is a hard gate.
        let cf = compile(
            r#"{
                "name": "D",
                "specifications": [
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "required": true,
                        "fields": [{"name": "value", "value": "x265"}]
                    },
                    {
                        "implementation": "ReleaseTitleSpecification",
                        "fields": [{"name": "value", "value": "HEVC"}]
                    }
                ]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);
        let hit = candidate("[Judas] Show - 01 [HEVC]", "Judas", 0, "");
        // Without required=true this matches (Example B); with it, no.
        assert!(!evaluate(&cf, &ctx(&hit, &cls, &hashes)));
    }

    #[test]
    fn example_e_negate_inverts_kernel() {
        let cf = compile(
            r#"{
                "name": "E",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "negate": true,
                    "fields": [{"name": "value", "value": "NoobSubs"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);

        // Kernel matches "NoobSubs", negate flips to false, CF fails.
        let noob = candidate("[NoobSubs] Show - 01 [8bit].mp4", "NoobSubs", 0, "");
        assert!(!evaluate(&cf, &ctx(&noob, &cls, &hashes)));

        // Kernel miss, negate flips to true, CF matches.
        let clean = candidate("[MTBB] Show - 01", "MTBB", 0, "");
        assert!(evaluate(&cf, &ctx(&clean, &cls, &hashes)));
    }

    #[test]
    fn example_f_negate_plus_required_blacklist_pattern() {
        // The standard TRaSH blacklist shape.
        let cf = compile(
            r#"{
                "name": "F",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "negate": true,
                    "required": true,
                    "fields": [{"name": "value", "value": "NoobSubs"}]
                }]
            }"#,
        );
        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);

        let clean = candidate("[MTBB] Show - 01", "MTBB", 0, "");
        assert!(evaluate(&cf, &ctx(&clean, &cls, &hashes)));

        let noob = candidate("[NoobSubs] Show - 01", "NoobSubs", 0, "");
        assert!(!evaluate(&cf, &ctx(&noob, &cls, &hashes)));
    }

    // ── Score summation ──────────────────────────────────────────────────

    #[test]
    fn total_cf_score_sums_only_matching_cfs() {
        let cf_x265 = compile(
            r#"{
                "name": "x265",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                }]
            }"#,
        );
        let mut cf_x265 = cf_x265;
        cf_x265.score = 500;

        let cf_anti_noob = compile(
            r#"{
                "name": "anti noob",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "negate": true,
                    "required": true,
                    "fields": [{"name": "value", "value": "NoobSubs"}]
                }]
            }"#,
        );
        let mut cf_anti_noob = cf_anti_noob;
        cf_anti_noob.score = -1000;

        let hashes = HashSet::new();
        let cls = classification(Source::Web, Resolution::R1080p);

        // MTBB x265: matches x265 (+500), matches anti-noob (-0? no,
        // anti-noob matches → its score -1000 also adds).
        let mtbb = candidate("[MTBB] Show - 01 [x265]", "MTBB", 0, "");
        let score = total_cf_score(&[cf_x265.clone(), cf_anti_noob.clone()], &ctx(&mtbb, &cls, &hashes));
        assert_eq!(score, 500 + (-1000));

        // NoobSubs (no x265): anti-noob fires negatively → CF doesn't
        // match → no -1000 contribution. x265 also doesn't match.
        let noob = candidate("[NoobSubs] Show - 01", "NoobSubs", 0, "");
        let score = total_cf_score(&[cf_x265, cf_anti_noob], &ctx(&noob, &cls, &hashes));
        assert_eq!(score, 0);
    }

    #[test]
    fn total_cf_score_empty_list_is_zero() {
        let hashes = HashSet::new();
        let cand = candidate("x", "", 0, "");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        assert_eq!(total_cf_score(&[], &ctx(&cand, &cls, &hashes)), 0);
    }

    // ── Vacuous-truth parity ─────────────────────────────────────────────

    #[test]
    fn empty_specs_cf_matches_every_release_strict_sonarr_parity() {
        // compile_from_json rejects this shape, but the evaluator still
        // has to handle it for strict Sonarr parity (pathologically
        // hand-edited state). Construct one manually.
        let cf = CompiledCustomFormat {
            id: 1,
            name: "empty".to_string(),
            score: 42,
            specs: vec![],
        };
        let hashes = HashSet::new();
        let cand = candidate("anything", "anyone", 0, "");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        assert!(evaluate(&cf, &ctx(&cand, &cls, &hashes)));
    }
}
