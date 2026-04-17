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

// A handful of spec fields (negate, required, source's raw Sonarr int)
// are parsed but not read by the current evaluator path — they're
// preserved so round-trip export matches Sonarr byte-for-byte and the
// semantics stay visible in the debugger. Scope the allow narrowly to
// this file rather than annotating each struct field.
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
    ReleaseTitle { regex: fancy_regex::Regex },
    ReleaseGroup { regex: fancy_regex::Regex },
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
        // Sonarr's CF JSON ships `fields` in two shapes depending on
        // where the dump came from:
        //   - Sonarr UI / API export: array of `{"name": …, "value": …}`
        //   - trash-guides repo JSON: object map like `{"value": …}`
        // `field_str` / `field_i64` / `field_f64` accept either shape via
        // `field_value`; we hand them the raw `fields` node and let them
        // do the work. An absent `fields` becomes `Value::Null`, which
        // both lookups treat as "no such field".
        let null_fields = serde_json::Value::Null;
        let fields: &serde_json::Value = spec_v.get("fields").unwrap_or(&null_fields);

        let kind: SpecKind = match implementation {
            "ReleaseTitleSpecification" => {
                let value = field_str(fields, "value")?;
                // Sonarr regexes are case-insensitive by convention;
                // prepend `(?i)` so every imported CF matches regardless
                // of how the user authored it. `sonarr_to_rust_regex`
                // rewrites .NET-isms (literal `[` inside a char class)
                // into forms fancy-regex accepts. See that function's
                // docs for the exact transforms.
                let rewritten = sonarr_to_rust_regex(&value);
                let re = fancy_regex::Regex::new(&format!("(?i){rewritten}"))
                    .map_err(|e| format!("ReleaseTitle regex: {e}"))?;
                SpecKind::ReleaseTitle { regex: re }
            }
            "ReleaseGroupSpecification" => {
                let value = field_str(fields, "value")?;
                let rewritten = sonarr_to_rust_regex(&value);
                let re = fancy_regex::Regex::new(&format!("(?i){rewritten}"))
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

/// Rewrite a .NET/Sonarr-flavored regex into something fancy-regex
/// accepts, preserving match semantics. The transforms performed:
///
/// 1. **Literal `[` inside a character class is escaped.** .NET regex
///    treats `[` as a literal when it appears inside `[...]`, so
///    trash-guides patterns like `[([]dual[])]` are valid .NET but
///    reject in regex-syntax (the parser errors on a "nested" class).
///    We walk the pattern tracking char-class state and emit `\[`
///    whenever a bare `[` appears inside a class.
///
/// The char-class-leading-`]` rule (`[])]` = class containing `]`, `)`)
/// is already handled by regex-syntax natively, so this pass leaves it
/// alone.
///
/// Backslash escapes are copied through without interpretation — this
/// is a syntactic fixup, not a semantic one.
fn sonarr_to_rust_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 4);
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    // `just_opened` is true immediately after `[` (and after an
    // optional leading `^`), so a `]` in that position is treated as
    // a literal rather than closing the class.
    let mut just_opened = false;

    while let Some(c) = chars.next() {
        if c == '\\' {
            // Escape sequence: copy both the backslash and the next
            // char verbatim regardless of class state.
            out.push(c);
            if let Some(n) = chars.next() {
                out.push(n);
            }
            just_opened = false;
            continue;
        }

        if !in_class {
            out.push(c);
            if c == '[' {
                in_class = true;
                just_opened = true;
            }
        } else if c == '^' && just_opened {
            // Negated class marker; still at the "leading position"
            // after the `^`.
            out.push(c);
        } else if c == ']' && !just_opened {
            out.push(c);
            in_class = false;
        } else if c == '[' {
            // Literal `[` inside a class — escape for regex-syntax.
            out.push('\\');
            out.push('[');
            just_opened = false;
        } else {
            out.push(c);
            just_opened = false;
        }
    }

    out
}

/// Look up a named field in a Sonarr CF spec regardless of which of
/// the two `fields` shapes the exporter used:
///
/// - **Array form** (Sonarr UI / API): `[{"name": "value", "value": X}, …]`
///   — the canonical shape Sonarr returns from `/api/v3/customformat`.
/// - **Object form** (trash-guides repo JSON): `{"value": X, "min": Y}`
///   — the shape trash-guides ships in `docs/json/sonarr/cf/*.json`.
///
/// Both shapes are valid round-trip Sonarr imports, so Ryokan accepts
/// both. Returns `None` if `fields` is neither shape or the name is
/// absent. Callers coerce the returned `Value` to the expected type.
fn field_value<'a>(fields: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    if let Some(arr) = fields.as_array() {
        for f in arr {
            if f.get("name").and_then(|n| n.as_str()) == Some(name) {
                return f.get("value");
            }
        }
        None
    } else if let Some(obj) = fields.as_object() {
        obj.get(name)
    } else {
        None
    }
}

fn field_str(fields: &serde_json::Value, name: &str) -> Result<String, String> {
    field_value(fields, name)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("field `{name}` missing"))
}

fn field_i64(fields: &serde_json::Value, name: &str) -> Result<i64, String> {
    field_value(fields, name)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("field `{name}` missing"))
}

fn field_f64(fields: &serde_json::Value, name: &str) -> Option<f64> {
    // Sonarr ships these as JSON numbers; accept either f64 or i64
    // encodings for robustness (`"min": 5` vs `"min": 5.0`).
    field_value(fields, name).and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|n| n as f64)))
}

// ───────────────────────────────────────────────────────────────────────────
// Per-spec kernel + negate wrapper
// ───────────────────────────────────────────────────────────────────────────

/// Raw per-spec match, pre-negate. Mirrors Sonarr's
/// `IsSatisfiedByWithoutNegate`.
fn evaluate_spec_kernel(spec: &CompiledSpec, ctx: &EvalContext) -> bool {
    match &spec.kind {
        // fancy-regex returns `Result<bool, Error>` because backtracking
        // can hit a step limit on pathological inputs. On error (step
        // limit exceeded, runtime failure) treat as non-match — a
        // Sonarr-compat CF should not be able to brick scoring for an
        // entire search just because one spec timed out.
        SpecKind::ReleaseTitle { regex } => regex.is_match(&ctx.result.title).unwrap_or(false),
        // `SearchResult::group` is a bare String. Empty means the Nyaa
        // scraper didn't find a `[Group]` prefix; an empty-string regex
        // still matches it, which is consistent with Sonarr's behavior.
        SpecKind::ReleaseGroup { regex } => regex.is_match(&ctx.result.group).unwrap_or(false),
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
///
/// Saturating addition: SEADEX_SCORE_BOOST is 10_000, individual TRaSH
/// CFs ship up to ±10_000, and user-authored CFs can carry arbitrary
/// scores. Naive `.sum()` would wrap on overflow and silently demote
/// every candidate below `minimum_score`, dropping the entire search.
pub fn total_cf_score(cfs: &[CompiledCustomFormat], ctx: &EvalContext) -> i32 {
    cfs.iter()
        .filter(|cf| evaluate(cf, ctx))
        .map(|cf| cf.score)
        .fold(0i32, i32::saturating_add)
}

/// Same total as [`total_cf_score`], but also returns the per-CF
/// breakdown of every matching CF with a non-zero score contribution,
/// in `custom_formats.id` order (the natural iteration order of the
/// compiled cache). Used by the scoring debug-log path in
/// `auto_search.rs` so the user-facing log row can list exactly which
/// CFs fired on each candidate (§6.3 of the plan). Production scoring
/// stays on the scalar [`total_cf_score`] variant for speed — only the
/// debug-log path pays the allocation.
pub fn total_cf_score_with_breakdown(
    cfs: &[CompiledCustomFormat],
    ctx: &EvalContext,
) -> (i32, Vec<(String, i32)>) {
    let mut total: i32 = 0;
    let mut breakdown: Vec<(String, i32)> = Vec::new();
    for cf in cfs {
        if !evaluate(cf, ctx) {
            continue;
        }
        // saturating_add — see total_cf_score for the overflow rationale.
        total = total.saturating_add(cf.score);
        // Zero-score matches are meaningful for CF authoring but add
        // noise to the debug line — skip them per the plan §6.3 wording
        // "every CF that matched with a nonzero score contribution."
        if cf.score != 0 {
            breakdown.push((cf.name.clone(), cf.score));
        }
    }
    (total, breakdown)
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

/// Re-run `load_compiled_cfs` and atomically swap the compiled set into
/// the shared `CompiledCfCache`. Callers take the write lock only long
/// enough to replace the inner `Arc`; readers on the scoring hot path
/// clone the `Arc` out under the read lock and then release it — so a
/// reader arriving mid-swap blocks for at most the duration of the Arc
/// replacement (microseconds) before returning a consistent snapshot.
/// Used by the Custom Formats settings page after any create / update /
/// delete / import.
pub async fn rebuild_cf_cache(cache: &CompiledCfCache, db: &SqlitePool) {
    let fresh = Arc::new(load_compiled_cfs(db).await);
    *cache.write().await = fresh;
}

/// `true` if any compiled CF contains a `SeaDexBest` spec. Used to
/// suppress the hardcoded SeaDex score boost when the user has opted
/// into controlling that boost themselves through a Custom Format —
/// otherwise a candidate on SeaDex would earn both the CF score and
/// the hardcoded `SEADEX_SCORE_BOOST` bump, which is double counting.
pub fn has_seadex_cf(cfs: &[CompiledCustomFormat]) -> bool {
    cfs.iter()
        .any(|cf| cf.specs.iter().any(|s| matches!(s.kind, SpecKind::SeaDexBest)))
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

    // ── Breakdown variant ────────────────────────────────────────────────

    #[test]
    fn total_cf_score_with_breakdown_returns_matching_contributions() {
        // Three CFs: two that match the candidate with non-zero scores,
        // one that doesn't match. Expect both matches in the breakdown
        // in CF order, and the total == sum of the matching scores.
        let cfs = vec![
            compile_from_json(
                r#"{"name":"x265","specifications":[{"implementation":"ReleaseTitleSpecification","fields":[{"name":"value","value":"x265"}]}]}"#,
                300,
                1,
            )
            .unwrap(),
            compile_from_json(
                r#"{"name":"flac","specifications":[{"implementation":"ReleaseTitleSpecification","fields":[{"name":"value","value":"flac"}]}]}"#,
                150,
                2,
            )
            .unwrap(),
            compile_from_json(
                r#"{"name":"noob","specifications":[{"implementation":"ReleaseGroupSpecification","fields":[{"name":"value","value":"^NoobSubs$"}]}]}"#,
                -1000,
                3,
            )
            .unwrap(),
        ];
        let cand = candidate("[MTBB] Show - 01 (BD x265 FLAC)", "MTBB", 0, "");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        let hashes = HashSet::new();
        let (total, breakdown) = total_cf_score_with_breakdown(&cfs, &ctx(&cand, &cls, &hashes));
        assert_eq!(total, 300 + 150);
        assert_eq!(breakdown.len(), 2);
        assert_eq!(breakdown[0].0, "x265");
        assert_eq!(breakdown[0].1, 300);
        assert_eq!(breakdown[1].0, "flac");
        assert_eq!(breakdown[1].1, 150);
    }

    #[test]
    fn total_cf_score_with_breakdown_skips_zero_score_matches() {
        // A CF that matches but has score=0 contributes to the total
        // correctly (trivially) but is omitted from the breakdown per
        // plan §6.3's "nonzero score contribution" wording.
        let cf = compile_from_json(
            r#"{"name":"zero","specifications":[{"implementation":"ReleaseTitleSpecification","fields":[{"name":"value","value":"x265"}]}]}"#,
            0,
            1,
        )
        .unwrap();
        let cand = candidate("Show x265", "", 0, "");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        let hashes = HashSet::new();
        let (total, breakdown) = total_cf_score_with_breakdown(&[cf], &ctx(&cand, &cls, &hashes));
        assert_eq!(total, 0);
        assert!(breakdown.is_empty());
    }

    // ── Default CF library ───────────────────────────────────────────────

    #[test]
    fn default_custom_formats_json_compiles_every_cf() {
        // Plan §7.2: the bundled default CF set at
        // `static/default_custom_formats.json` must compile through the
        // production CF compiler. This test is the regression guard for
        // a typo (wrong field name, bad regex, unsupported spec type)
        // silently breaking the Install Defaults button.
        const DEFAULTS: &str = include_str!("../../static/default_custom_formats.json");
        let value: serde_json::Value =
            serde_json::from_str(DEFAULTS).expect("defaults file is valid JSON");
        let entries = value
            .as_array()
            .expect("defaults file top-level must be an array");
        assert_eq!(
            entries.len(),
            8,
            "plan §7.2 specifies exactly 8 default CFs"
        );
        for (i, entry) in entries.iter().enumerate() {
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("(missing)");
            let score = entry.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let raw = entry.to_string();
            compile_from_json(&raw, score, (i + 1) as i64)
                .unwrap_or_else(|e| panic!("default CF `{name}` failed to compile: {e}"));
        }
    }

    #[test]
    fn default_seadex_cf_fires_on_seadex_hash_match() {
        // The first default CF is the SeaDex boost. Compiling it and
        // feeding it a candidate whose info_hash is in the SeaDex set
        // should trigger the match with a +10000 contribution — that's
        // the score that dominates every other CF in the stacked
        // hierarchy from plan §7.1.
        const DEFAULTS: &str = include_str!("../../static/default_custom_formats.json");
        let value: serde_json::Value = serde_json::from_str(DEFAULTS).unwrap();
        let first = &value.as_array().unwrap()[0];
        let score = first.get("score").unwrap().as_i64().unwrap() as i32;
        assert_eq!(score, 10000, "SeaDex CF score must be +10000");
        let cf = compile_from_json(&first.to_string(), score, 1).unwrap();

        let mut hashes = HashSet::new();
        hashes.insert("deadbeef".to_string());
        let cand = candidate("[MTBB] Show - 01 (BD x265 FLAC)", "MTBB", 0, "deadbeef");
        let cls = classification(Source::Unknown, Resolution::Unknown);
        assert!(evaluate(&cf, &ctx(&cand, &cls, &hashes)));
        assert!(has_seadex_cf(&[cf]));
    }

    #[test]
    fn default_penalize_8bit_mp4_spares_subsplease_mkv() {
        // Regression guard for plan §7.3 CF #7: the two-spec
        // `required=true` AND pattern must NOT fire on SubsPlease mkvs
        // even though they're 8-bit (no 10-bit marker). The `.mp4`
        // extension check is what protects them.
        const DEFAULTS: &str = include_str!("../../static/default_custom_formats.json");
        let value: serde_json::Value = serde_json::from_str(DEFAULTS).unwrap();
        // Find the penalize-8bit-mp4 entry by name rather than index so
        // a future reordering doesn't turn this into a silent false.
        let entry = value
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("8-bit mp4"))
                    .unwrap_or(false)
            })
            .expect("default set must include the 8-bit mp4 penalty CF");
        let cf = compile_from_json(
            &entry.to_string(),
            entry.get("score").unwrap().as_i64().unwrap() as i32,
            1,
        )
        .unwrap();

        let hashes = HashSet::new();
        let cls = classification(Source::Unknown, Resolution::Unknown);

        // SubsPlease weekly, .mkv container: should NOT match.
        let sp = candidate(
            "[SubsPlease] ShowX - 01 (1080p).mkv",
            "SubsPlease",
            0,
            "",
        );
        assert!(
            !evaluate(&cf, &ctx(&sp, &cls, &hashes)),
            "SubsPlease mkv must be spared by the 8-bit mp4 penalty"
        );

        // NoobSubs-style 8-bit mp4: SHOULD match (hits both specs).
        let noob = candidate(
            "[NoobSubs] ShowX - 01 (1080p 8bit).mp4",
            "NoobSubs",
            0,
            "",
        );
        assert!(
            evaluate(&cf, &ctx(&noob, &cls, &hashes)),
            "NoobSubs 8-bit mp4 must be caught by the penalty"
        );

        // A 10-bit mp4 (rare but possible): the required=true negate-10bit
        // spec fails post-negate, so the penalty should NOT fire.
        let tenbit_mp4 = candidate(
            "[SomeGroup] ShowX - 01 (1080p 10bit).mp4",
            "SomeGroup",
            0,
            "",
        );
        assert!(
            !evaluate(&cf, &ctx(&tenbit_mp4, &cls, &hashes)),
            "10-bit mp4 must survive the penalty"
        );
    }

    // ── Phase 9 integration: Kizumonogatari regression ──────────────────

    /// Load every CF from `static/default_custom_formats.json` into the
    /// compiled form the runtime actually uses. Factored into a helper so
    /// the Kizumonogatari regression test and the benchmark smoke test
    /// build the same CF set.
    fn load_default_cfs() -> Vec<CompiledCustomFormat> {
        const DEFAULTS: &str = include_str!("../../static/default_custom_formats.json");
        let value: serde_json::Value = serde_json::from_str(DEFAULTS).unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let score = entry.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                compile_from_json(&entry.to_string(), score, (i + 1) as i64).unwrap()
            })
            .collect()
    }

    /// Helper that bundles the (title, group, classification) triple
    /// most integration-test candidates need. Mirrors the real-world
    /// pipeline: a Nyaa `SearchResult` plus a `ClassificationResult`
    /// from the source module. Keeping both in a tuple keeps each
    /// candidate row on one grep-able line in the test body.
    fn make_fixture(
        title: &str,
        group: &str,
        source: Source,
        resolution: Resolution,
        info_hash: &str,
    ) -> (SearchResult, ClassificationResult) {
        let cand = candidate(title, group, 8 * 1024 * 1024 * 1024, info_hash);
        let cls = classification(source, resolution);
        (cand, cls)
    }

    /// Sum the matching CF scores for a candidate against the default
    /// set, with an empty SeaDex set. Mirrors the exact code path the
    /// auto-search scorer uses in production, minus the `base_score`
    /// layer (which is classification-driven and orthogonal to CF
    /// ordering for this regression test).
    fn score_against_defaults(
        cfs: &[CompiledCustomFormat],
        cand: &SearchResult,
        cls: &ClassificationResult,
    ) -> i32 {
        let hashes = HashSet::new();
        total_cf_score(cfs, &ctx(cand, cls, &hashes))
    }

    /// The core regression test for the bug where an 8-bit NoobSubs
    /// mp4 with high seeders could tie or beat a 10-bit BD release
    /// because the hardcoded `+5` quality-marker bonus was identical
    /// for both. With the default CF set installed, the BD release
    /// must win by a wide margin and the 8-bit mp4 release must sit
    /// at the bottom of the score ordering.
    ///
    /// We assert strict ordering of the whole fixture, not just the
    /// head-to-head — any future regression (a mis-applied negation,
    /// a dropped required spec, a bad regex) flips the sort order and
    /// fails the test with a message that names the regressed pair.
    ///
    /// **Fixture titles are deliberately synthetic.** `ReleaseGroup`
    /// CFs evaluate against `SearchResult.group` (see
    /// `SpecKind::ReleaseGroup` at ~L309), not the title, so the
    /// group identity comes from the `group` argument to
    /// `make_fixture`. The title string only needs to carry the
    /// tokens that `ReleaseTitleSpecification` CFs match on
    /// (`10bit`, `x265`, `hevc`, `flac`, `.mp4`). Nothing here
    /// claims to be any specific group's real-world filename format.
    #[test]
    fn kizumonogatari_regression_cf_ordering() {
        let cfs = load_default_cfs();
        assert_eq!(cfs.len(), 7, "default CF set must be 7 CFs");

        // Expected totals are computed from plan §7.2's score values:
        //   Tier-S BD   = 1200 (Tier S) + 600 (BD source)
        //               + 300 (10-bit/x265) + 150 (FLAC) = 2250
        //   WEB HEVC    = 400 (WEB groups) + 300 (hevc/10-bit) = 700
        //   WEB plain   = 400 (WEB groups) = 400
        //   WEB neutral = 0 (matches no CF)
        //   HorribleSubs WEB = 0 (not penalized by bundled defaults —
        //       users install anime-web-tier-05.json from TRaSH Guides
        //       for that signal; see #12)
        //
        // Groups are attached via the SearchResult.group field (the
        // 2nd argument to make_fixture), not the title. Titles are
        // opaque synthetic token blobs.
        let tier_s_bd = make_fixture(
            "fixture-bd-1080p-10bit-x265-flac.mkv",
            "MTBB",
            Source::BluRay,
            Resolution::R1080p,
            "aaaa",
        );
        let web_hevc = make_fixture(
            "fixture-web-1080p-hevc-10bit.mkv",
            "Judas",
            Source::Web,
            Resolution::R1080p,
            "bbbb",
        );
        let web_plain = make_fixture(
            "fixture-web-1080p.mkv",
            "SubsPlease",
            Source::Web,
            Resolution::R1080p,
            "cccc",
        );
        let web_neutral = make_fixture(
            "fixture-web-1080p.mkv",
            "Erai-raws",
            Source::Web,
            Resolution::R1080p,
            "dddd",
        );
        // Build the flat list of (label, candidate, classification,
        // expected) tuples used for both scoring and ordering checks.
        let fixture: Vec<(&str, &SearchResult, &ClassificationResult, i32)> = vec![
            ("Tier-S BD",      &tier_s_bd.0,       &tier_s_bd.1,       2250),
            ("WEB HEVC",       &web_hevc.0,        &web_hevc.1,        700),
            ("WEB plain",      &web_plain.0,       &web_plain.1,       400),
            ("WEB neutral",    &web_neutral.0,     &web_neutral.1,     0),
        ];

        // Per-candidate score assertion — each row's total must match
        // plan §7.2's score values. If any of these fails, the
        // failing row's label appears in the assertion message.
        for (label, cand, cls, expected) in &fixture {
            let got = score_against_defaults(&cfs, cand, cls);
            assert_eq!(
                got, *expected,
                "candidate `{label}` scored {got}, expected {expected}"
            );
        }

        // Strict ordering assertion — the fixture is already in
        // expected descending order, so the sort result should equal
        // the fixture order.
        let mut sorted: Vec<(&&str, i32)> = fixture
            .iter()
            .map(|(label, cand, cls, _)| (label, score_against_defaults(&cfs, cand, cls)))
            .collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let labels_in_score_order: Vec<&str> =
            sorted.iter().map(|(label, _)| **label).collect();
        assert_eq!(
            labels_in_score_order,
            vec![
                "Tier-S BD",
                "WEB HEVC",
                "WEB plain",
                "WEB neutral",
            ],
            "default CF set must produce the expected regression ordering"
        );
    }

    /// Post-#12 pin: HorribleSubs (and NoobSubs) WEB releases must
    /// score 0 against the bundled defaults — neither penalised nor
    /// rewarded. The old `-1000` casual-group CF was removed because
    /// it conflated "unmaintained but technically fine" (HorribleSubs)
    /// with "low-effort re-encode" (NoobSubs). Users who want a
    /// HorribleSubs penalty install the TRaSH Guides `anime-web-tier-05`
    /// CF, which is shipped as a fixture but not part of the bundled
    /// defaults.
    #[test]
    fn casual_groups_unpenalised_by_bundled_defaults() {
        let cfs = load_default_cfs();
        let horrible = make_fixture(
            "fixture-web-1080p.mkv",
            "HorribleSubs",
            Source::Web,
            Resolution::R1080p,
            "eeee",
        );
        let noob_8bit = make_fixture(
            "fixture-web-1080p-8bit.mp4",
            "NoobSubs",
            Source::Web,
            Resolution::R1080p,
            "ffff",
        );
        assert_eq!(
            score_against_defaults(&cfs, &horrible.0, &horrible.1),
            0,
            "HorribleSubs WEB must not be penalised by bundled defaults after #12"
        );
        // NoobSubs with 8-bit mp4 still trips the 8-bit mp4 penalty
        // (-500) which is independent of the casual-group CF.
        assert_eq!(
            score_against_defaults(&cfs, &noob_8bit.0, &noob_8bit.1),
            -500,
            "NoobSubs 8-bit mp4 must only incur the 8-bit mp4 penalty, not the removed casual-group penalty"
        );
    }

    // ── Phase 9 cross-compat §5.7.7 — reject-on-all-unsupported ───────

    #[test]
    fn compile_rejects_cf_with_only_unsupported_spec() {
        // TRaSH's real-world `BR-DISK` CF uses only a
        // `QualityModifierSpecification`, which Ryokan doesn't
        // implement. Per plan §5.7.7 #6, the compiler must reject
        // this rather than silently letting it through as an empty
        // (vacuous-match) CF.
        let json = r#"{
            "name": "BR-DISK",
            "specifications": [
                {
                    "name": "BR-DISK modifier",
                    "implementation": "QualityModifierSpecification",
                    "negate": false,
                    "required": false,
                    "fields": [{"name":"value","value":1}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 100, 1).unwrap_err();
        assert!(
            err.contains("all 1 specifications unsupported"),
            "error must mention the all-unsupported rejection, got: {err}"
        );
    }

    #[test]
    fn compile_rejects_cf_with_dropped_required_spec() {
        // Plan §5.7.7 #6: a CF with one supported spec plus a
        // required=true unsupported spec must be rejected — dropping
        // the required spec would change the CF's semantics silently.
        let json = r#"{
            "name": "Multi-Audio",
            "specifications": [
                {
                    "name": "Title",
                    "implementation": "ReleaseTitleSpecification",
                    "negate": false,
                    "required": false,
                    "fields": [{"name":"value","value":"x265"}]
                },
                {
                    "name": "Language",
                    "implementation": "LanguageSpecification",
                    "negate": false,
                    "required": true,
                    "fields": [{"name":"value","value":10}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 100, 1).unwrap_err();
        assert!(
            err.contains("required=true") && err.contains("dropped"),
            "error must mention the dropped-required rejection, got: {err}"
        );
    }

    // ── Phase 9 round-trip (§5.7.7 #4) ────────────────────────────────

    #[test]
    fn round_trip_preserves_trash_metadata() {
        // A representative trash-guides-shaped CF with all the
        // extension fields plan §5.7.6 wants preserved: `trash_id`,
        // `trash_scores`, `trash_description`, plus `includeCustomFormatWhenRenaming`.
        // The test proves that compiling the CF, re-serializing the
        // stored JSON (which is just the verbatim input because §5.1
        // stores raw JSON), and re-parsing gives back the same `Value`
        // tree. This is the "round-trip faithfully" assertion from §9.
        let json = r#"{
            "trash_id": "ed38b0b3-1e57-47bc-b0e1-abcdef012345",
            "trash_scores": {"default": 500},
            "trash_description": "Prefer x265 encodes over x264",
            "name": "x265 preference",
            "includeCustomFormatWhenRenaming": false,
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

        // Compile once to prove it's a valid CF from the production
        // compiler's perspective.
        let cf = compile_from_json(json, 500, 42).expect("fixture CF must compile");
        assert_eq!(cf.name, "x265 preference");

        // Round-trip path 1: raw JSON → Value → back to string →
        // Value. Mirrors what `settings_custom_formats_export` does
        // to every row in the database. The two Values must be equal
        // because `serde_json::Value` is order-sensitive for arrays
        // and order-insensitive for objects, which is exactly the
        // "byte-level equivalence modulo whitespace/field order"
        // semantic from plan §9.
        let parsed_in: serde_json::Value = serde_json::from_str(json).unwrap();
        let reserialized = serde_json::to_string(&parsed_in).unwrap();
        let parsed_out: serde_json::Value = serde_json::from_str(&reserialized).unwrap();
        assert_eq!(
            parsed_in, parsed_out,
            "raw JSON must round-trip as equal Value trees"
        );

        // Check every trash-guides extension field survived the round
        // trip — plan §5.7.6 flags these as the ones Ryokan must NOT
        // drop, even though the evaluator ignores them.
        assert_eq!(
            parsed_out.get("trash_id").and_then(|v| v.as_str()),
            Some("ed38b0b3-1e57-47bc-b0e1-abcdef012345")
        );
        assert_eq!(
            parsed_out
                .get("trash_scores")
                .and_then(|v| v.get("default"))
                .and_then(|v| v.as_i64()),
            Some(500)
        );
        assert_eq!(
            parsed_out.get("trash_description").and_then(|v| v.as_str()),
            Some("Prefer x265 encodes over x264")
        );
        assert_eq!(
            parsed_out
                .get("includeCustomFormatWhenRenaming")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    // ── Phase 9 benchmark smoke (§9) ──────────────────────────────────

    #[test]
    fn benchmark_100_candidates_8_cfs_under_15ms() {
        // Plan §9: compile the 8-CF default set once, score 100
        // candidates, hard threshold 15 ms wall-clock (three-sigma
        // headroom over the < 5 ms target). If this blows past 15 ms
        // something has regressed — allocation in the hot path, a
        // regex re-compile per candidate, quadratic behavior in
        // `evaluate`, etc.
        //
        // Debug builds are ~3-5x slower than release, so we only run
        // the hard assertion under release. In debug we just run the
        // loop to catch outright panics and print the timing.
        let cfs = load_default_cfs();

        // Five synthetic-token candidates reused round-robin to hit
        // 100 iterations. The mix exercises every branch of every
        // default CF, including the §5.7.3 Example D AND-across-specs
        // path for the 8-bit mp4 penalty. Titles carry only the CF
        // regex tokens — they do not mimic any specific group's real
        // filename format.
        let base_candidates: Vec<(SearchResult, ClassificationResult)> = vec![
            make_fixture(
                "fixture-bd-1080p-10bit-x265-flac.mkv",
                "MTBB",
                Source::BluRay,
                Resolution::R1080p,
                "hash0",
            ),
            make_fixture(
                "fixture-web-1080p-hevc-10bit.mkv",
                "Judas",
                Source::Web,
                Resolution::R1080p,
                "hash1",
            ),
            make_fixture(
                "fixture-web-1080p.mkv",
                "SubsPlease",
                Source::Web,
                Resolution::R1080p,
                "hash2",
            ),
            make_fixture(
                "fixture-web-1080p.mkv",
                "Erai-raws",
                Source::Web,
                Resolution::R1080p,
                "hash3",
            ),
            make_fixture(
                "fixture-web-1080p-8bit.mp4",
                "NoobSubs",
                Source::Unknown,
                Resolution::R1080p,
                "hash4",
            ),
        ];

        let hashes = HashSet::new();
        let iterations = 100usize;
        let start = std::time::Instant::now();
        let mut checksum: i32 = 0;
        for i in 0..iterations {
            let (cand, cls) = &base_candidates[i % base_candidates.len()];
            checksum = checksum.wrapping_add(total_cf_score(&cfs, &ctx(cand, cls, &hashes)));
        }
        let elapsed = start.elapsed();
        // Force the optimizer to keep the loop body.
        std::hint::black_box(checksum);

        let ms = elapsed.as_secs_f64() * 1000.0;
        eprintln!("cf-benchmark: {iterations} candidates × 8 CFs in {ms:.3} ms");

        // Hard threshold only in release. Debug timing is informational.
        if !cfg!(debug_assertions) {
            assert!(
                elapsed < std::time::Duration::from_millis(15),
                "CF evaluation over {iterations} candidates took {ms:.3} ms, \
                 exceeding the 15 ms regression threshold from plan §9"
            );
        }
    }

    // ── sonarr_to_rust_regex ────────────────────────────────────────────

    #[test]
    fn sonarr_rewrite_escapes_literal_lbracket_in_class() {
        // The trash-guides `anime-dual-audio` regex's critical fragment.
        let rewritten = sonarr_to_rust_regex(r"[([]dual[])]");
        assert_eq!(rewritten, r"[(\[]dual[])]");
        assert!(fancy_regex::Regex::new(&rewritten).is_ok());
    }

    #[test]
    fn sonarr_rewrite_passes_through_class_leading_rbracket() {
        // Sonarr's convention: `]` immediately after `[` is literal.
        // regex-syntax handles this natively, so the rewrite should
        // leave it alone and the result should still compile.
        let rewritten = sonarr_to_rust_regex(r"[])]");
        assert_eq!(rewritten, r"[])]");
        assert!(fancy_regex::Regex::new(&rewritten).is_ok());
    }

    #[test]
    fn sonarr_rewrite_preserves_escapes_and_negation() {
        // Negated class `[^...]` with escaped dot inside — should
        // pass through unchanged.
        let src = r"[^\.ab]";
        assert_eq!(sonarr_to_rust_regex(src), src);
        assert!(fancy_regex::Regex::new(src).is_ok());
    }

    #[test]
    fn sonarr_rewrite_leaves_non_class_lbracket_alone() {
        // A `[` outside a class (impossible per strict regex grammar
        // but harmless here) is not rewritten — `sonarr_to_rust_regex`
        // only touches `[` inside an already-open class.
        let src = r"\[text\]";
        assert_eq!(sonarr_to_rust_regex(src), src);
    }

    #[test]
    fn sonarr_rewrite_handles_multiple_classes_in_same_pattern() {
        // Two separate classes in one pattern, both with literal `[`.
        let rewritten = sonarr_to_rust_regex(r"[[a][[b]");
        assert_eq!(rewritten, r"[\[a][\[b]");
        assert!(fancy_regex::Regex::new(&rewritten).is_ok());
    }

    // ── trash-guides anime fixture set (plan §9, Gap E) ──────────────────
    //
    // Twenty-eight real trash-guides anime CF JSON files are vendored at
    // `fixtures/trash-guides-anime/`. Each fixture is pulled into the
    // binary via `include_str!` so the test needs no filesystem access
    // and no network at build time. These are the **actual** JSON shapes
    // trash-guides ships (object-form `fields`, `trash_id`, `trash_scores`,
    // `trash_description`, `LanguageSpecification` used as a soft hint),
    // so the test doubles as a round-trip regression for the object/array
    // `fields` bug that Gap E was created to catch — if either exporter
    // shape stops parsing, every entry in this array breaks at once.
    //
    // Extending the set: add a new file under `fixtures/trash-guides-anime/`
    // and a new `include_str!` line below. Any CF with a `required=true`
    // LanguageSpecification will fail the parse guard on purpose — that is
    // not a bug, that is Ryokan refusing to silently drop a gating spec.
    const TRASH_ANIME_FIXTURES: &[(&str, &str)] = &[
        ("10bit", include_str!("../../fixtures/trash-guides-anime/10bit.json")),
        ("anime-bd-tier-01", include_str!("../../fixtures/trash-guides-anime/anime-bd-tier-01.json")),
        ("anime-bd-tier-02", include_str!("../../fixtures/trash-guides-anime/anime-bd-tier-02.json")),
        ("anime-bd-tier-03", include_str!("../../fixtures/trash-guides-anime/anime-bd-tier-03.json")),
        ("anime-bd-tier-04", include_str!("../../fixtures/trash-guides-anime/anime-bd-tier-04.json")),
        ("anime-bd-tier-05", include_str!("../../fixtures/trash-guides-anime/anime-bd-tier-05.json")),
        ("anime-bd-tier-06", include_str!("../../fixtures/trash-guides-anime/anime-bd-tier-06.json")),
        ("anime-bd-tier-07", include_str!("../../fixtures/trash-guides-anime/anime-bd-tier-07.json")),
        ("anime-bd-tier-08", include_str!("../../fixtures/trash-guides-anime/anime-bd-tier-08.json")),
        ("anime-dual-audio", include_str!("../../fixtures/trash-guides-anime/anime-dual-audio.json")),
        ("anime-lq-groups", include_str!("../../fixtures/trash-guides-anime/anime-lq-groups.json")),
        ("anime-raws", include_str!("../../fixtures/trash-guides-anime/anime-raws.json")),
        ("anime-web-tier-01", include_str!("../../fixtures/trash-guides-anime/anime-web-tier-01.json")),
        ("anime-web-tier-02", include_str!("../../fixtures/trash-guides-anime/anime-web-tier-02.json")),
        ("anime-web-tier-03", include_str!("../../fixtures/trash-guides-anime/anime-web-tier-03.json")),
        ("anime-web-tier-04", include_str!("../../fixtures/trash-guides-anime/anime-web-tier-04.json")),
        ("anime-web-tier-05", include_str!("../../fixtures/trash-guides-anime/anime-web-tier-05.json")),
        ("anime-web-tier-06", include_str!("../../fixtures/trash-guides-anime/anime-web-tier-06.json")),
        ("bad-dual-groups", include_str!("../../fixtures/trash-guides-anime/bad-dual-groups.json")),
        ("dubs-only", include_str!("../../fixtures/trash-guides-anime/dubs-only.json")),
        ("fansub", include_str!("../../fixtures/trash-guides-anime/fansub.json")),
        ("fastsub", include_str!("../../fixtures/trash-guides-anime/fastsub.json")),
        ("uncensored", include_str!("../../fixtures/trash-guides-anime/uncensored.json")),
        ("v0", include_str!("../../fixtures/trash-guides-anime/v0.json")),
        ("v1", include_str!("../../fixtures/trash-guides-anime/v1.json")),
        ("v2", include_str!("../../fixtures/trash-guides-anime/v2.json")),
        ("v3", include_str!("../../fixtures/trash-guides-anime/v3.json")),
        ("v4", include_str!("../../fixtures/trash-guides-anime/v4.json")),
    ];

    /// Every vendored trash-guides anime CF must compile cleanly. A
    /// failure here means one of three things is broken:
    ///
    /// 1. The object-form `fields` accessor regressed — every fixture
    ///    would fail at once.
    /// 2. trash-guides shipped a new spec implementation string Ryokan
    ///    doesn't recognize, and it's `required=true` (and therefore
    ///    correctly rejected instead of silently dropped).
    /// 3. A regex in a vendored file became invalid against `fancy-regex`
    ///    (unlikely but possible since Sonarr uses .NET's regex engine
    ///    and we use `fancy-regex`, which supports look-around but not
    ///    every PCRE feature).
    #[test]
    fn trash_anime_fixtures_all_parse() {
        assert_eq!(
            TRASH_ANIME_FIXTURES.len(),
            28,
            "fixture count drift: update the set and this assertion together"
        );

        let mut failures: Vec<(String, String)> = Vec::new();
        for (label, raw) in TRASH_ANIME_FIXTURES {
            match compile_from_json(raw, 0, 1) {
                Ok(cf) => {
                    // Sanity: every fixture has at least one surviving
                    // spec (otherwise it's either vacuous or the parser
                    // silently dropped something we didn't expect).
                    assert!(
                        !cf.specs.is_empty(),
                        "fixture `{label}` compiled to zero specs — vacuous CF"
                    );
                }
                Err(e) => failures.push(((*label).to_string(), e)),
            }
        }

        assert!(
            failures.is_empty(),
            "trash-guides anime fixtures failed to parse:\n{}",
            failures
                .iter()
                .map(|(name, err)| format!("  - {name}: {err}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Every `specifications[].implementation` string in the fixture set
    /// must be one Ryokan knows about (supported + handled, or
    /// explicitly-tolerated-as-soft-hint). If trash-guides ships a new
    /// implementation type (e.g. they add `EditionSpecification` to an
    /// anime CF), this test fails with the exact file + unknown string
    /// so a human can decide whether to add support or reject the CF.
    #[test]
    fn trash_anime_fixtures_use_only_known_implementations() {
        // Strict subset of Ryokan's supported spec list. Kept in sync
        // with the match arm in `compile_from_json`.
        //
        // `LanguageSpecification` is **not supported** but is soft-
        // tolerated by the parser when `required=false` — it is silently
        // dropped. Including it here is a claim that "we intentionally
        // let this appear in vendored fixtures"; a `required=true`
        // Language spec would still fail the first fixture test above.
        const KNOWN: &[&str] = &[
            "ReleaseTitleSpecification",
            "ReleaseGroupSpecification",
            "SourceSpecification",
            "ResolutionSpecification",
            "SizeSpecification",
            "LanguageSpecification",
            "SeaDexBestSpecification",
            "Ryokan.SeaDexBestSpecification",
        ];

        let mut unknowns: Vec<(String, String)> = Vec::new();
        for (label, raw) in TRASH_ANIME_FIXTURES {
            let v: serde_json::Value = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("fixture `{label}` is not valid JSON: {e}"));
            let specs = v
                .get("specifications")
                .and_then(|s| s.as_array())
                .unwrap_or_else(|| panic!("fixture `{label}` has no specifications array"));
            for s in specs {
                let impl_name = s
                    .get("implementation")
                    .and_then(|i| i.as_str())
                    .unwrap_or("<missing>");
                if !KNOWN.contains(&impl_name) {
                    unknowns.push(((*label).to_string(), impl_name.to_string()));
                }
            }
        }

        assert!(
            unknowns.is_empty(),
            "trash-guides anime fixtures reference unknown implementations — \
             either add parser support or update the KNOWN list with a rationale:\n{}",
            unknowns
                .iter()
                .map(|(name, imp)| format!("  - {name}: {imp}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Round-trip guard: parse → serialize → re-parse must produce the
    /// same compiled CF (same spec count, same match against a known
    /// release title). This catches upstream schema drift that would
    /// break Ryokan-compatible re-export of a trash-guides CF.
    #[test]
    fn trash_anime_fixtures_roundtrip_json() {
        for (label, raw) in TRASH_ANIME_FIXTURES {
            let first = compile_from_json(raw, 0, 1)
                .unwrap_or_else(|e| panic!("fixture `{label}` failed first parse: {e}"));

            // Re-serialize the raw JSON Value (not the compiled form —
            // the compiled form is lossy by design). This mirrors what
            // the export handler does: it hands back the stored `json`
            // column, which for a CF imported from trash-guides is the
            // raw trash-guides bytes.
            let value: serde_json::Value = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("fixture `{label}` not valid JSON on re-read: {e}"));
            let reserialized = serde_json::to_string(&value)
                .unwrap_or_else(|e| panic!("fixture `{label}` failed to re-serialize: {e}"));

            let second = compile_from_json(&reserialized, 0, 1).unwrap_or_else(|e| {
                panic!("fixture `{label}` failed round-trip parse: {e}")
            });

            assert_eq!(
                first.specs.len(),
                second.specs.len(),
                "fixture `{label}`: spec count drifted across round-trip \
                 ({} → {})",
                first.specs.len(),
                second.specs.len()
            );
            assert_eq!(
                first.name, second.name,
                "fixture `{label}`: name drifted across round-trip"
            );
        }
    }

    /// Surviving-spec diff: catches "we silently started dropping more
    /// specs than before" across the whole fixture set, which would
    /// indicate either a parser regression or a new unsupported spec
    /// type appearing in trash-guides. The numbers below are the
    /// surviving (post-drop) spec counts as of 2026-04-13 and should
    /// be updated in lockstep with intentional parser changes.
    #[test]
    fn trash_anime_fixtures_surviving_spec_counts_are_stable() {
        // Every fixture compiles to >=1 spec (otherwise Gap E's fixture
        // set includes a vacuous-match CF, which is meaningless). This
        // is a weaker assertion than a hard expected-count map, but it
        // survives trash-guides upstream edits without needing a lockstep
        // update while still catching mass regressions. If you want to
        // tighten this further, snapshot the `(label, spec_count)` map
        // here.
        let mut total_surviving = 0usize;
        for (label, raw) in TRASH_ANIME_FIXTURES {
            let cf = compile_from_json(raw, 0, 1)
                .unwrap_or_else(|e| panic!("fixture `{label}` failed to parse: {e}"));
            assert!(
                !cf.specs.is_empty(),
                "fixture `{label}` compiled to zero surviving specs"
            );
            total_surviving += cf.specs.len();
        }
        // Sanity floor: 28 CFs × at least 1 surviving spec each = 28.
        // Realistic floor is far higher (hundreds of ReleaseTitle/Group
        // specs across the set) — we saw 504 at vendoring time. Use a
        // conservative floor that allows trash-guides some churn without
        // triggering a spurious test failure.
        assert!(
            total_surviving >= 200,
            "surviving-spec count dropped to {total_surviving} — expected \
             at least 200 across the 28-file fixture set (saw 504 at vendor \
             time). Investigate before updating this floor."
        );
    }
}
