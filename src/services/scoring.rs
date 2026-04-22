use std::collections::HashSet;
use std::sync::LazyLock;

use regex_lite::Regex;
use serde::{Deserialize, Serialize};

use crate::services::custom_formats::{self, CompiledCustomFormat, EvalContext};
use crate::services::nyaa::{SearchOptions, SearchResult};
use crate::services::source::{ClassificationResult, Resolution, Source, WebKind};

// Word-boundary "dub" / "dubbed" — anchors prevent the prior bare-
// substring match from false-positiving on "redub", "dubsoon",
// "dubbing", or release tags that happen to contain those bytes.
static DUB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:dub|dubbed)\b").expect("dub regex compiles"));

/// One line of a score breakdown: what fired, what it contributed,
/// and an optional human-readable detail (which group matched, how
/// many seeders, what threshold crossed, etc.). Surfaced on the
/// /api/search response and persisted alongside grab history so the
/// "why this score" UI can show users exactly what happened.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ScoreComponent {
    /// Short label — displayed as the left-column "what" in the UI
    /// breakdown table (e.g. "Seeders", "Preferred Group",
    /// "Resolution Match").
    pub label: String,
    /// Signed point contribution. Positive = bonus, negative =
    /// penalty. Sum of all `delta`s in a breakdown equals the total
    /// score — invariant pinned by tests.
    pub delta: i32,
    /// Optional free-text detail for the UI tooltip / secondary row
    /// ("3 of N preferred", "1000+ seeders", etc.). Keeps the label
    /// concise while still giving users the full picture.
    pub detail: Option<String>,
}

impl ScoreComponent {
    fn new(label: &str, delta: i32, detail: Option<String>) -> Self {
        Self {
            label: label.to_string(),
            delta,
            detail,
        }
    }
}

/// Score a search result based on multiple factors.
/// `prefer_subs` controls whether dual audio/dub releases are penalized (default true).
#[allow(dead_code)]
pub fn score_result(r: &SearchResult, opts: &SearchOptions) -> i32 {
    score_result_with_sub_pref(r, opts, true)
}

/// Scalar score. Delegates to `score_result_with_breakdown` and
/// discards the component list — use the breakdown variant directly
/// when you need both total and per-component detail.
pub fn score_result_with_sub_pref(
    r: &SearchResult,
    opts: &SearchOptions,
    prefer_subs: bool,
) -> i32 {
    score_result_with_breakdown(r, opts, prefer_subs).0
}

/// Same total as `score_result_with_sub_pref`, plus the ordered
/// list of components that contributed to the score. Invariant:
/// `breakdown.iter().map(|c| c.delta).sum::<i32>() == total`.
///
/// Components are emitted in the evaluation order (seeders first,
/// then group, resolution, and so on). Zero-delta checks are
/// omitted — a "no preferred group configured, didn't penalize"
/// non-event doesn't add noise to the UI. The invariant holds
/// because we only push when we actually mutate `score`.
#[allow(clippy::cognitive_complexity)]
pub fn score_result_with_breakdown(
    r: &SearchResult,
    opts: &SearchOptions,
    prefer_subs: bool,
) -> (i32, Vec<ScoreComponent>) {
    let mut total: i32 = 0;
    let mut parts: Vec<ScoreComponent> = Vec::new();
    let mut add = |label: &str, delta: i32, detail: Option<String>| {
        total += delta;
        parts.push(ScoreComponent::new(label, delta, detail));
    };

    // Seeders.
    if r.seeders > 100 {
        add("Seeders", 30, Some(format!("{} seeders (>100)", r.seeders)));
    } else if r.seeders > 50 {
        add("Seeders", 25, Some(format!("{} seeders (>50)", r.seeders)));
    } else if r.seeders > 10 {
        add("Seeders", 20, Some(format!("{} seeders (>10)", r.seeders)));
    } else if r.seeders > 0 {
        add("Seeders", 10, Some(format!("{} seeders", r.seeders)));
    } else {
        add("Seeders", -10, Some("zero seeders".to_string()));
    }

    // Preferred group. Earlier entries are stronger preferences.
    if !opts.preferred_groups.is_empty() {
        if !r.group.is_empty() {
            let mut matched_index = None;
            for (idx, g) in opts.preferred_groups.iter().enumerate() {
                if g.eq_ignore_ascii_case(&r.group) {
                    matched_index = Some(idx as i32);
                    break;
                }
            }
            if let Some(idx) = matched_index {
                let delta = 140 - (idx * 20);
                add(
                    "Preferred Group",
                    delta,
                    Some(format!("[{}] rank {} of preferred list", r.group, idx + 1)),
                );
            } else {
                add(
                    "Non-Preferred Group",
                    -15,
                    Some(format!("[{}] not in preferred list", r.group)),
                );
            }
        } else {
            add(
                "No Group Tag",
                -10,
                Some("release title has no [Group] prefix".to_string()),
            );
        }
    }

    // Preferred resolution.
    if !opts.preferred_resolution.is_empty() && r.resolution == opts.preferred_resolution {
        add(
            "Preferred Resolution",
            20,
            Some(format!("{} matches preferred", r.resolution)),
        );
    }

    // Batch bonus.
    if r.is_batch {
        add("Batch Release", 15, None);
    }

    // Trusted bonus.
    if r.is_trusted {
        add("Trusted Uploader", 10, None);
    }

    // Encoding/source quality.
    let lower = r.title.to_lowercase();
    if lower.contains("10bit")
        || lower.contains("10-bit")
        || lower.contains("x265")
        || lower.contains("hevc")
        || lower.contains("bluray")
        || lower.contains("blu-ray")
        || lower.contains("bdrip")
        || lower.contains(" bd ")
        || lower.starts_with("bd ")
        || lower.contains("[bd")
        || lower.contains("(bd")
    {
        add(
            "Encoding / Source Quality",
            5,
            Some("10bit / x265 / HEVC / BluRay keyword in title".to_string()),
        );
    }

    // Dub vs Sub scoring.
    //
    // Detecting the bare substring `"multi"` false-positived on titles
    // that contained words like "multimedia" or group/release tags
    // ending in "multi" — those got tagged as dual-audio and shifted
    // under the sub/dub preference logic, nudging scoring in the wrong
    // direction. Tighten to the actual release-naming conventions for
    // multi-audio releases.
    let is_dual = lower.contains("dual audio")
        || lower.contains("dual.audio")
        || lower.contains("multi audio")
        || lower.contains("multi.audio")
        || lower.contains("multi-audio")
        || lower.contains("multiaudio");
    // Match "dub"/"dubbed" only as whole words. The earlier `multi`
    // tightening missed this companion case — bare contains("dub")
    // would fire on "redub", "dubsoon", and any release tag whose bytes
    // happened to include "dub". `english dub` stays as a literal
    // substring because the space anchors it.
    let is_dub = is_dual || DUB_RE.is_match(&lower) || lower.contains("english dub");
    if prefer_subs {
        if is_dub {
            add(
                "Dub / Dual Audio Penalty",
                -15,
                Some("user prefers subs; release flagged as dub/dual".to_string()),
            );
        }
    } else if is_dub {
        add(
            "Dub / Dual Audio Bonus",
            15,
            Some("user prefers dubs".to_string()),
        );
    }

    // Downloads popularity.
    if r.downloads > 10000 {
        add(
            "Downloads",
            15,
            Some(format!("{} downloads (>10k)", r.downloads)),
        );
    } else if r.downloads > 5000 {
        add(
            "Downloads",
            10,
            Some(format!("{} downloads (>5k)", r.downloads)),
        );
    } else if r.downloads > 1000 {
        add(
            "Downloads",
            5,
            Some(format!("{} downloads (>1k)", r.downloads)),
        );
    }

    // Small batch bonus (under ~25GB).
    if r.is_batch && r.size_bytes > 0 && r.size_bytes < 25 * 1024 * 1024 * 1024 {
        add("Compact Batch", 10, Some("batch under 25 GiB".to_string()));
    }

    (total, parts)
}

/// Rehydrate a `ClassificationResult` from the already-populated source
/// fields on a `SearchResult`. The scraper stores `source` / `resolution`
/// / `web_kind` as display strings plus `is_remux` / `is_bdmv` booleans;
/// the CF evaluator wants the typed enums. `evidence` / `confidence` /
/// `needs_review` / `decision_rule` aren't available at manual-search
/// time, so they default — CF evaluation doesn't read them.
fn classification_from_search_result(r: &SearchResult) -> ClassificationResult {
    let web_kind = if r.web_kind.is_empty() {
        WebKind::Unknown
    } else {
        WebKind::from_str(&r.web_kind)
    };
    ClassificationResult {
        source: Source::from_str(&r.source),
        resolution: Resolution::from_str(&r.resolution),
        is_remux: r.is_remux,
        web_kind,
        is_bdmv: r.is_bdmv,
        confidence: 1.0,
        needs_review: false,
        evidence: Vec::new(),
        decision_rule: crate::services::source::DecisionRule::Empty,
    }
}

/// Evaluate the compiled CF set against each result in `results`,
/// adding matching CF contributions to both `result.score` and
/// `result.score_breakdown`. Used by the manual-search path so the
/// "why this score" expander shows CF deltas alongside the base rules.
///
/// `seadex_hashes` can be empty — the manual search has no series
/// context, so SeaDex specs simply never fire. That's fine for now;
/// the manual search isn't the SeaDex surface.
///
/// Appends one `ScoreComponent` per matching CF with a non-zero score,
/// labeled `"CF: <name>"` so the UI can distinguish them from the
/// base-score rules at a glance.
pub fn apply_cf_breakdown(
    results: &mut [SearchResult],
    cfs: &[CompiledCustomFormat],
    seadex_hashes: &HashSet<String>,
) {
    if cfs.is_empty() {
        return;
    }
    for r in results.iter_mut() {
        let classification = classification_from_search_result(r);
        // Borrowed ctx needs the result to live for the whole call,
        // but we're about to mutate the result's score. Capture the
        // breakdown first with an immutable borrow, drop it, then
        // mutate.
        let (cf_total, breakdown) = {
            let ctx = EvalContext {
                result: r,
                classification: &classification,
                seadex_hashes,
            };
            custom_formats::total_cf_score_with_breakdown(cfs, &ctx)
        };
        if cf_total == 0 && breakdown.is_empty() {
            continue;
        }
        r.score = r.score.saturating_add(cf_total);
        for (name, delta) in breakdown {
            r.score_breakdown
                .push(ScoreComponent::new(&format!("CF: {name}"), delta, None));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::nyaa::{SearchOptions, SearchResult};

    fn result(seeders: i32, title: &str) -> SearchResult {
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
        }
    }

    fn opts() -> SearchOptions {
        SearchOptions::default()
    }

    #[test]
    fn breakdown_sum_equals_total_always() {
        // Invariant: components[].delta.sum() == total_score.
        // Exercise a handful of realistic shapes to pin it.
        let cases: Vec<(SearchResult, SearchOptions, bool)> = vec![
            (
                result(150, "[SubsPlease] Frieren - 01 (1080p)"),
                opts(),
                true,
            ),
            (result(0, "No Seeders Release"), opts(), true),
            (
                {
                    let mut r = result(55, "[Kaizoku] Series - Batch [1080p BluRay x265]");
                    r.is_batch = true;
                    r.is_trusted = true;
                    r.size_bytes = 10 * 1024 * 1024 * 1024;
                    r.downloads = 15_000;
                    r.group = "Kaizoku".to_string();
                    r
                },
                SearchOptions {
                    preferred_groups: vec!["Kaizoku".to_string(), "smol".to_string()],
                    preferred_resolution: "1080p".to_string(),
                    ..SearchOptions::default()
                },
                true,
            ),
            (
                {
                    let mut r = result(5, "[Group] Series - 01 Dual Audio (1080p)");
                    r.group = "Group".to_string();
                    r
                },
                SearchOptions {
                    preferred_groups: vec!["smol".to_string()],
                    preferred_resolution: "1080p".to_string(),
                    ..SearchOptions::default()
                },
                true,
            ),
        ];
        for (r, opts_case, prefer_subs) in cases {
            let (total, parts) = score_result_with_breakdown(&r, &opts_case, prefer_subs);
            let sum: i32 = parts.iter().map(|c| c.delta).sum();
            assert_eq!(
                total, sum,
                "invariant violated for {:?} — total={} sum={} parts={:?}",
                r.title, total, sum, parts
            );
            // Every component should have a non-zero delta (we don't
            // emit no-op entries).
            for p in &parts {
                assert_ne!(p.delta, 0, "zero-delta component: {:?}", p);
            }
        }
    }

    #[test]
    fn scalar_score_matches_breakdown_total() {
        // The two public APIs must agree on the total.
        let mut r = result(75, "[Group] Cool Series - 01 (1080p) [BD].mkv");
        r.group = "Group".to_string();
        r.is_batch = false;
        r.downloads = 3000;
        let opts = SearchOptions {
            preferred_groups: vec!["Group".to_string()],
            preferred_resolution: "1080p".to_string(),
            ..SearchOptions::default()
        };
        let scalar = score_result_with_sub_pref(&r, &opts, true);
        let (breakdown_total, _) = score_result_with_breakdown(&r, &opts, true);
        assert_eq!(scalar, breakdown_total);
    }

    #[test]
    fn apply_cf_breakdown_noop_with_empty_cf_list() {
        let mut r = result(30, "[Group] Series - 01 (1080p)");
        r.score = 42;
        let before_breakdown = r.score_breakdown.len();
        let mut batch = vec![r];
        apply_cf_breakdown(&mut batch, &[], &HashSet::new());
        assert_eq!(batch[0].score, 42);
        assert_eq!(batch[0].score_breakdown.len(), before_breakdown);
    }

    #[test]
    fn apply_cf_breakdown_appends_cf_prefixed_entries_and_bumps_score() {
        // Compile a tiny CF that matches any release whose title
        // contains "x265". Using the real parser so this test stays
        // honest about how CF scoring actually fires.
        let cf = crate::services::custom_formats::compile_from_json(
            r#"{
                "name": "x265 bonus",
                "specifications": [{
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                }]
            }"#,
            50,
            1,
        )
        .expect("test CF compiles");

        let mut hit = result(10, "[Group] Series - 01 (1080p) [x265].mkv");
        hit.score = 20;
        let base_breakdown_len = hit.score_breakdown.len();

        let mut miss = result(10, "[Group] Series - 02 (1080p).mkv");
        miss.score = 20;

        let mut batch = vec![hit, miss];
        apply_cf_breakdown(&mut batch, std::slice::from_ref(&cf), &HashSet::new());

        // Hit: score bumped, one new "CF: x265 bonus" entry.
        assert_eq!(batch[0].score, 70);
        assert_eq!(batch[0].score_breakdown.len(), base_breakdown_len + 1);
        let added = batch[0].score_breakdown.last().expect("new entry");
        assert_eq!(added.label, "CF: x265 bonus");
        assert_eq!(added.delta, 50);

        // Miss: untouched.
        assert_eq!(batch[1].score, 20);
    }

    #[test]
    fn preferred_group_rank_appears_in_detail() {
        let mut r = result(10, "[Beatrice-Raws] Series - 01 (1080p)");
        r.group = "Beatrice-Raws".to_string();
        let opts = SearchOptions {
            preferred_groups: vec!["smol".to_string(), "Beatrice-Raws".to_string()],
            preferred_resolution: String::new(),
            ..SearchOptions::default()
        };
        let (_, parts) = score_result_with_breakdown(&r, &opts, true);
        let group_comp = parts
            .iter()
            .find(|c| c.label == "Preferred Group")
            .expect("preferred group component missing");
        assert_eq!(group_comp.delta, 120); // 140 - (1 * 20)
        assert!(
            group_comp
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("rank 2")
        );
    }
}
