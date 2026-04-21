//! Title-normalization, alias collection, and target-matching.
//!
//! The query builder turns an `AnimeDetail` into a set of alias strings,
//! then `matches_target` filters Nyaa releases down to those that plausibly
//! describe the same series. Sibling-rejection keeps sequels / prequels /
//! arcs from false-positive matching via `SiblingRejectPrecompute` +
//! `sibling_match_rejects`.

use std::collections::HashSet;

use crate::services::anilist::AnimeDetail;

use super::{
    SearchTarget, episode_match, is_pack_candidate_relation, parse_release_numbers, season_mismatch,
};

pub fn normalize_title(input: &str) -> String {
    let lower = input.to_lowercase();
    let mut cleaned = String::with_capacity(lower.len());
    let mut in_brackets = 0i32;

    for ch in lower.chars() {
        match ch {
            '[' | '(' | '{' => in_brackets += 1,
            ']' | ')' | '}' => in_brackets = (in_brackets - 1).max(0),
            _ if in_brackets > 0 => continue,
            _ if ch.is_alphanumeric() || ch.is_whitespace() => cleaned.push(ch),
            _ => cleaned.push(' '),
        }
    }

    cleaned
        .split_whitespace()
        .filter(|token| {
            !matches!(
                *token,
                "1080p"
                    | "720p"
                    | "2160p"
                    | "webrip"
                    | "web"
                    | "bluray"
                    | "aac"
                    | "hevc"
                    | "x265"
                    | "x264"
                    | "dual"
                    | "audio"
                    | "multisub"
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn token_set(value: &str) -> HashSet<String> {
    value
        .split_whitespace()
        // Keep single-character tokens only when they're numeric —
        // single digits like "0" in "Jujutsu Kaisen 0" are the only
        // thing distinguishing the prequel movie's sibling alias from
        // the base franchise's own alias "Jujutsu Kaisen", so dropping
        // them lets sibling_match_rejects tie on tokens and fail to
        // reject the movie release for an S1 episode target. Single
        // alphabetic characters (stray "a", "I", "N") remain filtered
        // out because they carry no disambiguation value.
        .filter(|token| token.len() > 1 || token.chars().all(|c| c.is_ascii_digit()))
        .map(|token| token.to_string())
        .collect()
}

pub fn token_overlap_ratio(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let common = a.intersection(b).count() as f32;
    common / b.len() as f32
}

pub fn collect_aliases(detail: &AnimeDetail) -> Vec<String> {
    dedupe_strings(vec![
        detail.title_romaji.clone(),
        detail.title_english.clone(),
        detail.title_native.clone(),
    ])
}

/// Distinctive titles of this series' siblings (sequels, prequels, side
/// stories, alternative versions, spin-offs, summaries) — used to reject
/// releases that look MORE like a sibling than the target.
///
/// The motivating bug: auto-searching for Jujutsu Kaisen S1 E6 grabbed a
/// release titled `[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen -
/// 06`, which is actually an S2/S3 arc. The existing season_mismatch()
/// heuristic only catches explicit `S02` / `Season 2` markers; an arc
/// title like "Shimetsu Kaiyuu" slips through. But AniList knows that
/// "Jujutsu Kaisen: Shimetsu Kaiyuu" is a SEQUEL of JJK S1 — so we can
/// use the relation graph to derive the distinctive tokens that, when
/// present in a release filename, mean "this is the sibling, not me".
///
/// Returns sibling titles only where the sibling's normalized title is
/// NOT a substring of any of this target's own aliases (otherwise the
/// sibling title would match the target too — e.g. a prequel sharing
/// the base franchise name is not a useful discriminator). The returned
/// titles are still raw (un-normalized) so the matching logic can
/// re-normalize them the same way it does the release title.
pub fn collect_sibling_aliases(detail: &AnimeDetail, own_aliases: &[String]) -> Vec<String> {
    if detail.id <= 0 || detail.relations.is_empty() {
        return Vec::new();
    }

    // Normalized own-alias set — used to filter out sibling titles that
    // are themselves substrings of one of our own aliases (those would
    // substring-match us too, so they're not distinctive).
    let normalized_own: Vec<String> = own_aliases
        .iter()
        .map(|a| normalize_title(a))
        .filter(|s| !s.is_empty())
        .collect();

    let mut out: Vec<String> = Vec::new();
    for rel in &detail.relations {
        if !rel.media_type.eq_ignore_ascii_case("ANIME") {
            continue;
        }
        if !is_pack_candidate_relation(&rel.relation_type) {
            continue;
        }
        // Consider all three title fields so romaji-only or native-only
        // titles still contribute. The de-dup below squashes repeats.
        for raw in [
            rel.title_english.as_str(),
            rel.title_romaji.as_str(),
            rel.title_native.as_str(),
        ] {
            if raw.is_empty() {
                continue;
            }
            let normalized = normalize_title(raw);
            // Need ≥ 2 tokens for the sibling title to be a meaningful
            // discriminator — a single token is too generic and will
            // false-positive on unrelated releases that happen to share
            // a common word.
            if normalized.split_whitespace().count() < 2 {
                continue;
            }
            // Skip sibling titles whose normalized form is a substring
            // of one of our own aliases — those can't tell us apart
            // from the target.
            if normalized_own.iter().any(|own| own.contains(&normalized)) {
                continue;
            }
            out.push(raw.to_string());
        }
    }
    dedupe_strings(out)
}

/// Precomputed normalized token sets for the own-alias and sibling-alias
/// lists used by [`sibling_match_rejects`]. Built once per target sweep
/// (per call to `find_all_for_target` / `collect_scored_for_target` /
/// `collect_scored_batches_for_target`) and reused across every release
/// candidate the sweep checks against the target, instead of re-running
/// `normalize_title` + `token_set` on the same alias strings ~50×
/// (candidates) per target. Pure perf hoist — the rejection semantics
/// are identical to the prior per-call implementation.
#[derive(Debug, Clone, Default)]
pub struct SiblingRejectPrecompute {
    /// Token sets for own aliases. Used to find the best target-alias
    /// overlap with any release — a sibling only wins if it beats this
    /// number strictly.
    own_token_sets: Vec<HashSet<String>>,
    /// Sibling entries as `(normalized_title, token_set)` pairs. The
    /// normalized title is kept alongside its token set so the
    /// contiguous-substring fallback has a stable, deterministic string
    /// to match against (the old implementation rebuilt this from
    /// `HashSet::iter()` per call, which is nondeterministic order and
    /// would silently misbehave on contiguous-substring checks).
    siblings: Vec<(String, HashSet<String>)>,
}

impl SiblingRejectPrecompute {
    pub fn build(own_aliases: &[String], sibling_aliases: &[String]) -> Self {
        let own_token_sets = own_aliases
            .iter()
            .map(|a| token_set(&normalize_title(a)))
            .collect();
        let siblings = sibling_aliases
            .iter()
            .filter_map(|s| {
                let normalized = normalize_title(s);
                let tokens = token_set(&normalized);
                if tokens.is_empty() {
                    None
                } else {
                    Some((normalized, tokens))
                }
            })
            .collect();
        Self {
            own_token_sets,
            siblings,
        }
    }
}

/// Reject a release when it looks MORE like one of our siblings than
/// it does like us. The check compares token overlap: if any sibling
/// alias shares strictly more tokens with the release than the best
/// target alias does, the release is for the sibling.
///
/// Returns `true` to reject, `false` to keep.
///
/// Called from `matches_target` and the interactive-search path. Both
/// are guarded by an upstream basic alias-match, so by the time we get
/// here the release already passes the "could plausibly be us" gate —
/// the sibling check is the last defense against "plausibly us" also
/// being "more plausibly a sibling".
pub(super) fn sibling_match_rejects(
    normalized_release: &str,
    normalized_release_tokens: &HashSet<String>,
    precompute: &SiblingRejectPrecompute,
) -> bool {
    if precompute.siblings.is_empty() {
        return false;
    }

    // Best token overlap COUNT between release and any of our own aliases.
    // Using absolute overlap count (not ratio) so a sibling with 4 matching
    // tokens beats a target alias with 2 matching tokens even if the target
    // alias has fewer tokens overall.
    let best_own_overlap: usize = precompute
        .own_token_sets
        .iter()
        .map(|tokens| normalized_release_tokens.intersection(tokens).count())
        .max()
        .unwrap_or(0);

    for (normalized_sibling, sibling_tokens) in &precompute.siblings {
        let sibling_overlap = normalized_release_tokens
            .intersection(sibling_tokens)
            .count();
        // Strictly greater: a tie means both the target and the sibling
        // match equally well, which is the normal case for a release
        // like "Jujutsu Kaisen - 06" where sibling "Jujutsu Kaisen 2nd
        // Season" also overlaps on {jujutsu, kaisen}. Only reject when
        // the sibling picks up EXTRA tokens that the target doesn't.
        if sibling_overlap > best_own_overlap {
            // Also require that the sibling's entire normalized title
            // is either a contiguous substring of the release or that
            // ALL of its tokens appear in the release. This prevents
            // freak two-token overlaps ("side story" + some other
            // common fragment) from tripping the rejection.
            let all_tokens_present = sibling_tokens
                .iter()
                .all(|t| normalized_release_tokens.contains(t));
            if all_tokens_present || normalized_release.contains(normalized_sibling.as_str()) {
                return true;
            }
        }
    }
    false
}

/// Extended aliases: synonyms + decomposed sub-phrases from compound titles.
/// Only used as a fallback when primary aliases don't find results.
pub fn collect_extended_aliases(detail: &AnimeDetail) -> Vec<String> {
    let primary = collect_aliases(detail);
    let mut extra = Vec::new();

    // Add AniList synonyms.
    extra.extend(detail.synonyms.iter().cloned());

    // Decompose all titles (primary + synonyms) into sub-phrases.
    // Nyaa releases often use just the subtitle portion
    // (e.g. "Steel Ball Run" from "JoJo's Bizarre Adventure: Part 7–Steel Ball Run").
    let all_titles: Vec<String> = primary.iter().chain(extra.iter()).cloned().collect();
    for title in &all_titles {
        for segment in split_title_segments(title) {
            extra.push(segment);
        }
    }

    // Return only the NEW aliases (not already in primary).
    let primary_lower: HashSet<String> = primary.iter().map(|s| s.to_lowercase()).collect();
    dedupe_strings(extra)
        .into_iter()
        .filter(|s| !primary_lower.contains(&s.to_lowercase()))
        .collect()
}

/// Split a compound title on common delimiters and return meaningful segments.
/// Filters out segments that are too short or too generic to be useful search
/// terms.
///
/// Segments are used both as Nyaa search queries AND as matching aliases
/// inside `matches_target`, which means an over-generic segment can
/// substring-match unrelated shows on Nyaa and cause a completely wrong
/// grab. A single-word subtitle (especially a common English word or
/// hyphenated phrase) is almost always ambiguous — it will substring-match
/// any release that happens to contain the word, regardless of whether
/// that release is for this show or an unrelated one with the same word
/// in its name.
///
/// The 2-token minimum is the cheap defense: segments with only one
/// whitespace-separated token are rejected, regardless of length, because
/// they can't be trusted to uniquely identify a show. Segments with 2+
/// tokens remain — those are specific enough that substring-matching them
/// against an unrelated release is vanishingly unlikely.
fn split_title_segments(title: &str) -> Vec<String> {
    // Normalize various dash types to a common delimiter for splitting.
    let normalized = title
        .replace(['–', '—'], "|") // en dash and em dash
        .replace(": ", "|") // colon+space (keep "Re:Zero" intact)
        .replace(" - ", "|");

    let mut segments = Vec::new();
    for part in normalized.split('|') {
        let trimmed = part.trim();
        // Skip segments that are too short or just "Part N" / "Season N".
        if trimmed.len() < 5 {
            continue;
        }
        if trimmed.eq_ignore_ascii_case(title.trim()) {
            continue;
        }
        // Require at least 2 whitespace-separated tokens. Single-word
        // segments are too generic to use as matching aliases: they can
        // substring-match any release title that happens to contain the
        // word (see doc comment above for the Kizumonogatari / Gundam
        // Iron-Blooded Orphans incident).
        if trimmed.split_whitespace().count() < 2 {
            continue;
        }
        // Skip pure numbering like "Part 7", "Season 2", "2nd Season".
        let lower = trimmed.to_lowercase();
        if lower.starts_with("part ") && lower.len() < 10 {
            continue;
        }
        if lower.starts_with("season ") && lower.len() < 12 {
            continue;
        }
        if lower.ends_with(" season") && lower.len() < 14 {
            continue;
        }
        segments.push(trimmed.to_string());
    }
    segments
}

pub fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(trimmed.to_string());
        }
    }
    out
}

pub fn matches_target(
    title: &str,
    aliases: &[String],
    sibling_precompute: &SiblingRejectPrecompute,
    target: &SearchTarget,
    expected_season: i32,
    allow_batch_episode: bool,
    absolute_offset: i32,
) -> bool {
    let normalized_title = normalize_title(title);
    let title_tokens = token_set(&normalized_title);

    let alias_match = aliases.iter().any(|alias| {
        let normalized_alias = normalize_title(alias);
        normalized_title.contains(&normalized_alias)
            || token_overlap_ratio(&title_tokens, &token_set(&normalized_alias)) >= 0.6
    });

    if !alias_match {
        return false;
    }

    // Sibling rejection: if the release looks more like a sequel /
    // prequel / side story than it looks like us, reject. See the
    // JJK S1→S3 case in the `collect_sibling_aliases` docstring.
    if sibling_match_rejects(&normalized_title, &title_tokens, sibling_precompute) {
        return false;
    }

    match target {
        SearchTarget::Single => true,
        SearchTarget::Episode(target_ep) => {
            // Season check: reject if release has an explicit season that doesn't match
            if season_mismatch(title, expected_season) {
                return false;
            }

            let parsed = parse_release_numbers(title);
            if parsed.is_empty() {
                return false;
            }
            // Reject releases with 3+ episode numbers (batch/multi-episode)
            // unless the caller explicitly allows batch-to-episode matching
            // (used for quality upgrade searches where BD season packs are the
            // only source for higher-quality individual episodes).
            if !allow_batch_episode && parsed.len() > 2 {
                return false;
            }
            // #30 — Accept either the relative (AL-own) or the absolute
            // (SubsPlease-style) episode number. See `episode_match`
            // for the details.
            episode_match(&parsed, *target_ep, absolute_offset)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // split_title_segments uses a 2-token minimum to reject segments that
    // are too generic to safely become matching aliases. These tests cover
    // the rule in isolation with abstract inputs so the behavior is
    // described, not tied to any particular show.

    #[test]
    fn split_segments_keeps_three_token_subtitle() {
        let segments = split_title_segments("Main Title: Sub One Two Three");
        assert!(
            segments.iter().any(|s| s == "Sub One Two Three"),
            "multi-word subtitle should be kept as a segment, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_keeps_two_token_subtitle() {
        // Two whitespace-separated tokens is the minimum.
        let segments = split_title_segments("Main Title: Alpha Beta");
        assert!(
            segments.iter().any(|s| s == "Alpha Beta"),
            "two-token subtitle should be kept, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_rejects_single_word_subtitle() {
        let segments = split_title_segments("Main Title: Singleword");
        assert!(
            !segments.iter().any(|s| s == "Singleword"),
            "single-word subtitle should be rejected, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_rejects_hyphenated_single_word() {
        // Hyphens are not whitespace, so "Hyphen-Word" is still one token
        // under the rule — important because hyphenated English phrases
        // like "Iron-Blooded" are common enough to substring-match many
        // unrelated titles.
        let segments = split_title_segments("Main Title: Hyphen-Word");
        assert!(
            !segments.iter().any(|s| s == "Hyphen-Word"),
            "hyphenated single-word segment should be rejected, got {:?}",
            segments
        );
    }

    #[test]
    fn split_segments_keeps_multi_word_main_portion() {
        // Even when the subtitle is rejected, the leading multi-word
        // portion of a compound title remains usable.
        let segments = split_title_segments("Main Title Two: Singleword");
        assert!(
            segments.iter().any(|s| s == "Main Title Two"),
            "multi-word leading portion should be kept, got {:?}",
            segments
        );
    }

    #[test]
    fn matches_target_rejects_release_whose_only_overlap_is_a_rejected_segment() {
        // End-to-end regression: a release whose token overlap with the
        // primary alias is below the 0.6 threshold must not slip through
        // just because some single-word substring of a synonym happens to
        // appear in the release filename. With the 2-token rule in place,
        // that single-word substring is never produced as an alias, so
        // substring-match can't succeed.
        let aliases = vec![
            "Main Title: Subtitle One".to_string(),
            "Main Title: Subtitle Two".to_string(),
        ];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let unrelated_release =
            "[Group] Totally Different Show - Subtitle One-Word Thing - 01 [1080p].mkv";
        // The release shares only the word "Subtitle" with the primary
        // alias tokens {main, title, subtitle, one} / {main, title,
        // subtitle, two}. Overlap ratio for either alias is 1/4 = 0.25,
        // well below 0.6. No segment derived from the primary aliases
        // survives the 2-token rule to substring-match "Subtitle" in
        // isolation, so the match must fail.
        assert!(
            !matches_target(
                unrelated_release,
                &aliases,
                &no_siblings,
                &SearchTarget::Episode(1),
                0,
                false,
                0
            ),
            "unrelated release should not match via token overlap alone"
        );
    }

    #[test]
    fn matches_target_accepts_release_with_full_primary_alias_substring() {
        let aliases = vec!["Main Title Subtitle One".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&aliases, &[]);
        let good_release = "[Group] Main Title Subtitle One [BD 1080p].mkv";
        assert!(matches_target(
            good_release,
            &aliases,
            &no_siblings,
            &SearchTarget::Single,
            0,
            false,
            0
        ));
    }

    #[test]
    fn matches_target_rejects_sibling_arc_release() {
        // Regression: auto-searching JJK S1 E6 used to grab
        // `[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06`
        // because the sibling arc title has no explicit "S02"/"Season 2"
        // marker for `season_mismatch` to catch, but "Jujutsu Kaisen" is
        // a substring of the release. The sibling check resolves this:
        // the sibling alias "Jujutsu Kaisen: Shimetsu Kaiyuu" has 4
        // overlapping tokens with the release vs the target's 2, so the
        // sibling wins and the release is rejected.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let siblings = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 [1080p CR WEBRip HEVC AAC].mkv";
        assert!(
            !matches_target(
                release,
                &own,
                &precompute,
                &SearchTarget::Episode(6),
                1,
                false,
                0
            ),
            "sibling arc release must not match the base-franchise target"
        );
    }

    #[test]
    fn matches_target_keeps_base_franchise_release_despite_siblings() {
        // Symmetric: with the same sibling list, a plain JJK S1 release
        // should still match the target. The sibling overlaps on only
        // 2 tokens ({jujutsu, kaisen}) — the same as the target's own
        // overlap — so the sibling check is a no-op.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let siblings = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen - 06 [1080p].mkv";
        assert!(matches_target(
            release,
            &own,
            &precompute,
            &SearchTarget::Episode(6),
            1,
            false,
            0
        ));
    }

    #[test]
    fn matches_target_keeps_target_arc_release_against_unrelated_sibling() {
        // A JJK S2 Shibuya Incident target should still accept its own
        // arc release even when the sibling list includes another arc.
        let own = vec!["Jujutsu Kaisen: Shimetsu Kaiyuu".to_string()];
        let siblings = vec![
            "Jujutsu Kaisen".to_string(),
            "Jujutsu Kaisen: Kaigyoku Gyokusetsu".to_string(),
        ];
        let precompute = SiblingRejectPrecompute::build(&own, &siblings);
        let release = "[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 [1080p].mkv";
        assert!(matches_target(
            release,
            &own,
            &precompute,
            &SearchTarget::Episode(6),
            0,
            false,
            0
        ));
    }

    // ── #30 — absolute-vs-relative Nyaa episode numbering ──────────────
    //
    // SubsPlease (and others) number sequel cours either as the AL-own
    // relative number ("Otonari S2 - 03") or as the absolute number
    // continuing from S1 ("Jujutsu Kaisen - 56" for JJK S3 E9,
    // "Re Zero - 68" for a post-S2 episode). Before #30 the filter was
    // strict-relative, so the absolute releases were dropped from both
    // interactive and auto search.
    //
    // `episode_match` is the shared check used by both paths; these
    // tests pin both numbering conventions against realistic parsed
    // sets, and then verify the public `matches_target` applies the
    // same rule end-to-end.

    fn parsed(nums: &[i32]) -> std::collections::HashSet<i32> {
        nums.iter().copied().collect()
    }

    #[test]
    fn episode_match_accepts_relative_number_without_offset() {
        // First-season / offset=0: only the relative number counts,
        // which matches the legacy strict-relative behavior.
        assert!(episode_match(&parsed(&[3]), 3, 0));
        assert!(!episode_match(&parsed(&[25]), 3, 0));
    }

    #[test]
    fn episode_match_accepts_relative_number_even_when_offset_set() {
        // SubsPlease "Otonari no Tenshi-sama S2 - 03" (relative
        // numbering) against a target with an S1 prequel of 12
        // episodes must still pass — relative numbering is the more
        // common convention and we can't know which one any given
        // release picked.
        assert!(episode_match(&parsed(&[3]), 3, 12));
    }

    #[test]
    fn episode_match_accepts_absolute_number_against_relative_target() {
        // JJK S3 E9 ships as "Jujutsu Kaisen - 56" — absolute numbering
        // continuing from S1 (24) + S2 (23) = 47 prior cour episodes.
        assert!(episode_match(&parsed(&[56]), 9, 47));
        // Re:Zero - 68 is another realistic example.
        assert!(episode_match(&parsed(&[68]), 18, 50));
    }

    #[test]
    fn episode_match_rejects_unrelated_numbers_with_offset() {
        // An absolute number from a different episode is still wrong.
        // Target is S3 E9 (= absolute 56); release is absolute 60
        // (= S3 E13) — rejected.
        assert!(!episode_match(&parsed(&[60]), 9, 47));
        // Target is S3 E1 (= absolute 48); release is relative 5 — rejected.
        assert!(!episode_match(&parsed(&[5]), 1, 47));
    }

    #[test]
    fn matches_target_accepts_subsplease_absolute_numbered_sequel_cour() {
        // Full-path regression: a SubsPlease absolute-numbered release
        // for JJK S3 E9 must pass through `matches_target` when the
        // cumulative S1+S2 offset (47) is supplied.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&own, &[]);
        let release = "[SubsPlease] Jujutsu Kaisen - 56 (1080p) [0F106B43].mkv";
        assert!(matches_target(
            release,
            &own,
            &no_siblings,
            &SearchTarget::Episode(9),
            // `expected_season` is the season_mismatch target, not the
            // numbering target — this is 3 because we're asking for S3.
            3,
            false,
            47,
        ));
    }

    #[test]
    fn matches_target_rejects_absolute_numbered_sibling_cour_against_wrong_target() {
        // Mirror of the above: a release carrying an absolute number
        // that doesn't line up with our target (even once the offset
        // is added) must still be rejected. target = S3 E1
        // (absolute 48); release is absolute 60 = S3 E13 — wrong
        // episode.
        let own = vec!["Jujutsu Kaisen".to_string()];
        let no_siblings = SiblingRejectPrecompute::build(&own, &[]);
        let release = "[SubsPlease] Jujutsu Kaisen - 60 (1080p).mkv";
        assert!(!matches_target(
            release,
            &own,
            &no_siblings,
            &SearchTarget::Episode(1),
            3,
            false,
            47,
        ));
    }
}
