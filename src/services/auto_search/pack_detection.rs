//! Pack-file sibling-series detection + transitive-relation graft.
//!
//! When a multi-series pack lands (Monogatari split saga, JoJo P3/P4 box,
//! Rebuild of Evangelion 1.0/2.0/3.0), Ryokan needs to route each file to
//! the correct AniList entry. This module owns:
//!
//! - [`detect_sibling_entries_in_pack`] — the sibling detector
//! - [`compute_sibling_episode_offset`] — absolute-vs-relative numbering math
//! - [`transitive_relation_graft`] + [`expand_parent_with_transitive_relations`]
//!   — extends the direct relation graph by one hop to catch split sagas
//!   AniList doesn't link directly (Monogatari — Owari 2nd Season).
//! - [`is_transitive_walk_source`] / [`is_pack_candidate_relation`]
//!   — relation-type classifiers driving the walk + filter.
//! - [`SiblingMatch`] / [`TRANSITIVE_WALK_MAX_FETCHES`] — types + limits.

use crate::services::anilist::{AnimeDetail, RelatedEntry};
use crate::services::media;

use super::{is_media_filename, normalize_subtitle, trailing_subtitle_of, within_episode_slack};

pub struct SiblingMatch {
    pub anilist_id: i64,
    pub mal_id: Option<i64>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
    pub cover_url: String,
    pub format: String,
    pub status: String,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    /// The subtitle that produced the match (e.g. "Stardust
    /// Crusaders"). Logged at grab time so the operator can see *why*
    /// a sibling was picked up.
    pub matched_subtitle: String,
    /// Indices into the torrent's file list of files that belong to
    /// this sibling. Each file index is unique across the full return
    /// value — [`detect_sibling_entries_in_pack`] resolves overlaps by
    /// longest-subtitle-wins.
    pub file_indices: Vec<usize>,
    /// How many episodes to subtract from each file's parsed episode
    /// number before treating it as an episode of this sibling.
    ///
    /// Set to `min_ep - 1` (where `min_ep` is the smallest parsed
    /// episode number in this sibling's files) when the sibling uses
    /// numbering continuous with the parent — e.g. a 20-ep
    /// Owarimonogatari batch with `S07E14 - Owarimonogatari Second
    /// Season` resolves E14 to local ep 1 via offset 13. Set to 0
    /// when the sibling's files use their own arc-local numbering
    /// (e.g. an Egypt-hen pack with `Stardust Crusaders S03E01`
    /// filenames already starting at 1).
    ///
    /// Detection rule: if `min(sibling_file_ep_nums) > parent_cap`,
    /// offset = `min_ep - 1`; otherwise offset = 0. Using `min_ep - 1`
    /// rather than `parent_cap` is what makes BD-split cases work,
    /// where the release partitioned the parent into more files than
    /// AL's episode count (merged long-runtime episodes get split,
    /// pushing the sibling's first episode past `parent_cap + 1`).
    /// Computed per-sibling regardless of whether the match came from
    /// the subtitle path or the episode-range fallback.
    pub episode_offset: i32,
}

/// Relation types we'll consider in-pack candidates when scanning an
/// AniList relation graph for siblings. Excludes:
///
/// - **Source material** (`ADAPTATION`, `SOURCE`, `COMPILATION`,
///   `CONTAINS`) — these point at manga / LN / book entries that
///   will never appear in an anime torrent.
/// - **Off-series tie-ins** (`CHARACTER`, `OTHER`) — `CHARACTER`
///   links to shared-universe spinoffs that share no animation DNA
///   with the parent (e.g. a crossover cameo), and `OTHER` is
///   AniList's dumping ground for unusual relations (promotional
///   videos, live-action adaptations, disambiguation links, etc.).
///   Both are noisy enough that including them mostly pads the
///   candidate list with entries that never match.
///
/// Everything else — `SEQUEL`, `PREQUEL`, `SIDE_STORY`, `PARENT`,
/// `ALTERNATIVE`, `SPIN_OFF`, `SUMMARY` — passes through because the
/// downstream subtitle-match + episode-count cap are already doing
/// the real false-positive filtering. This gate is a performance
/// filter that avoids normalizing obviously-wrong candidates against
/// every filename.
pub(super) fn is_pack_candidate_relation(relation_type: &str) -> bool {
    !matches!(
        relation_type,
        "ADAPTATION" | "SOURCE" | "COMPILATION" | "CONTAINS" | "CHARACTER" | "OTHER"
    )
}

/// Maximum number of depth-1 AniList fetches per auto-expand grab.
/// Bounds our AL API usage so a deeply-linked franchise doesn't blow
/// past the background rate limits when we try to graft transitive
/// relations onto the parent's relation list. Any franchise needing
/// more than 10 hops out from the parent is almost certainly a
/// signal-to-noise loss anyway.
pub const TRANSITIVE_WALK_MAX_FETCHES: usize = 10;

/// Which of `parent.relations`' edges warrant walking one hop
/// further to graft their own relations onto the parent? This is a
/// NARROWER gate than [`is_pack_candidate_relation`] because each
/// step we take is an extra AL fetch — we only chase franchise-core
/// links (not SUMMARY, not ALTERNATIVE / alternate retellings) to
/// avoid pulling in genuinely unrelated entries that happen to be
/// loosely associated.
///
/// Motivating case: Owarimonogatari (AL 21262) does not list
/// Owarimonogatari 2nd Season (AL 99423) in its direct relations,
/// but its PREQUEL Monogatari Series Second Season does reach it
/// via the saga's shared continuation graph. Walking PREQUEL
/// outward from the parent is what lets sibling detection find it.
pub fn is_transitive_walk_source(relation_type: &str) -> bool {
    matches!(
        relation_type,
        "PREQUEL" | "SEQUEL" | "PARENT" | "SIDE_STORY" | "SPIN_OFF"
    )
}

/// Build the set of depth-1 transitive relations to graft onto
/// `parent.relations`. For every direct relation whose type passes
/// [`is_transitive_walk_source`] and which appears in
/// `neighbor_details` (the caller is responsible for fetching the
/// map from AL), pull in that neighbor's OWN relations — filtered
/// through [`is_pack_candidate_relation`] and de-duplicated against
/// the parent id plus every id already present in `parent.relations`.
///
/// Pure function: takes a pre-fetched neighbor map so it's
/// deterministic and testable without live AniList. The caller is
/// responsible for honoring [`TRANSITIVE_WALK_MAX_FETCHES`] when
/// populating the map.
///
/// # Why a graft (not a rewalk)
///
/// We don't recursively walk the graph — one hop is enough to catch
/// the Monogatari / JoJo-style missing-edge cases without pulling in
/// unrelated franchises. Going depth-2 would start grafting e.g.
/// side-story side-stories that have nothing to do with the grabbed
/// pack, and each extra hop is another AL API call per grab.
pub fn transitive_relation_graft(
    parent: &AnimeDetail,
    neighbor_details: &std::collections::HashMap<i64, AnimeDetail>,
) -> Vec<RelatedEntry> {
    if parent.id <= 0 {
        return Vec::new();
    }
    // Track ids we've already accepted to avoid duplicates and
    // cycles. Seed with the parent id and every direct-relation id
    // so a neighbor's relation list can't re-add either.
    let mut seen: std::collections::HashSet<i64> =
        std::collections::HashSet::with_capacity(parent.relations.len() + 1);
    seen.insert(parent.id);
    for direct in &parent.relations {
        seen.insert(direct.id);
    }

    let mut graft: Vec<RelatedEntry> = Vec::new();
    for direct in &parent.relations {
        if !is_transitive_walk_source(&direct.relation_type) {
            continue;
        }
        if !direct.media_type.eq_ignore_ascii_case("ANIME") {
            continue;
        }
        let Some(neighbor_detail) = neighbor_details.get(&direct.id) else {
            continue;
        };
        for hop in &neighbor_detail.relations {
            if hop.id <= 0 {
                continue;
            }
            if !seen.insert(hop.id) {
                continue;
            }
            if !hop.media_type.eq_ignore_ascii_case("ANIME") {
                continue;
            }
            if !is_pack_candidate_relation(&hop.relation_type) {
                continue;
            }
            graft.push(hop.clone());
        }
    }
    graft
}

/// Return a cloned `parent` whose `relations` vec has been extended
/// by [`transitive_relation_graft`]. When the graft is empty this
/// still returns a clone so the caller can pass a single
/// `&AnimeDetail` into sibling detection regardless of whether the
/// walk found anything.
pub fn expand_parent_with_transitive_relations(
    parent: &AnimeDetail,
    neighbor_details: &std::collections::HashMap<i64, AnimeDetail>,
) -> AnimeDetail {
    let graft = transitive_relation_graft(parent, neighbor_details);
    if graft.is_empty() {
        return parent.clone();
    }
    tracing::debug!(
        "auto-expand: transitive walk grafted {} additional relation(s) onto parent id={}",
        graft.len(),
        parent.id
    );
    let mut expanded = parent.clone();
    expanded.relations.extend(graft);
    expanded
}

/// Detect sibling anime entries (sequel / prequel / side story /
/// etc. of the parent) whose own episodes are present in a megapack
/// release's file list.
///
/// **Provenance gate:** returns an empty `Vec` when
/// `parent_detail.id <= 0`. Negative IDs are the Jikan fallback
/// sentinel (`-mal_id`) and non-positive IDs are not AniList entries.
/// Jikan's relations scrape reflects MAL's graph, and MAL splits
/// sagas that AniList merges (Stone Ocean is 3 MAL entries vs 1 AL
/// entry), so auto-adding MAL siblings against an AL-sourced parent
/// would duplicate library rows. When AL is down, the grab still
/// proceeds — it just skips sibling expansion — and the background
/// 12h metadata refresh will retroactively run detection the next
/// time AL returns the relation list.
///
/// **Overlap resolution:** when a filename matches more than one
/// sibling subtitle (e.g. "Stardust" ⊂ "Stardust Crusaders", or a
/// freak collision between two unrelated sibling titles), the longer
/// normalized subtitle wins. Each file index appears in exactly one
/// `SiblingMatch::file_indices` across the return value.
///
/// **Episode-count cap:** each sibling's match set is rejected if it
/// overshoots the sibling's own AniList `episodes` count by ×1.5 + 2.
/// Matches with `episodes: None` bypass the cap (airing series, which
/// the downstream grab path handles anyway).
///
/// Callers get a best-effort list — siblings whose title has no
/// trailing subtitle (e.g. a franchise root like "Naruto Shippuden")
/// are silently skipped, matching the conservative behavior of
/// `pick_wanted_file_indices`.
pub fn detect_sibling_entries_in_pack(
    filenames: &[String],
    parent_detail: &AnimeDetail,
) -> Vec<SiblingMatch> {
    if parent_detail.id <= 0 {
        return Vec::new();
    }

    let parent_label = if !parent_detail.title_english.is_empty() {
        parent_detail.title_english.as_str()
    } else {
        parent_detail.title_romaji.as_str()
    };
    tracing::debug!(
        "auto-expand: detect_siblings starting parent='{}' parent_anilist_id={} parent_episodes={:?} relations={} files={}",
        parent_label,
        parent_detail.id,
        parent_detail.episodes,
        parent_detail.relations.len(),
        filenames.len()
    );

    // Candidates: one entry per relation that produced a usable
    // subtitle. Stored by index into `parent_detail.relations` to
    // avoid borrowing complications during the materialize pass.
    let mut candidates: Vec<(usize, String, String)> = Vec::new(); // (rel_idx, raw subtitle, normalized needle)
    for (rel_idx, rel) in parent_detail.relations.iter().enumerate() {
        let rel_label = if !rel.title_english.is_empty() {
            rel.title_english.as_str()
        } else {
            rel.title_romaji.as_str()
        };
        if !rel.media_type.eq_ignore_ascii_case("ANIME") {
            tracing::debug!(
                "auto-expand: subtitle skip rel='{}' reason=media_type={}",
                rel_label,
                rel.media_type
            );
            continue;
        }
        if !is_pack_candidate_relation(&rel.relation_type) {
            tracing::debug!(
                "auto-expand: subtitle skip rel='{}' reason=relation_type={}",
                rel_label,
                rel.relation_type
            );
            continue;
        }
        let sibling_title = if !rel.title_english.is_empty() {
            rel.title_english.as_str()
        } else if !rel.title_romaji.is_empty() {
            rel.title_romaji.as_str()
        } else {
            tracing::debug!(
                "auto-expand: subtitle skip rel_idx={} reason=no-title relation_type={}",
                rel_idx,
                rel.relation_type
            );
            continue;
        };
        let Some(subtitle) = trailing_subtitle_of(sibling_title) else {
            tracing::debug!(
                "auto-expand: subtitle skip rel='{}' reason=no-trailing-subtitle relation_type={}",
                sibling_title,
                rel.relation_type
            );
            continue;
        };
        let needle = normalize_subtitle(&subtitle);
        if needle.is_empty() {
            tracing::debug!(
                "auto-expand: subtitle skip rel='{}' reason=empty-needle subtitle='{}'",
                sibling_title,
                subtitle
            );
            continue;
        }
        tracing::debug!(
            "auto-expand: subtitle candidate rel='{}' relation_type={} subtitle='{}' needle='{}'",
            sibling_title,
            rel.relation_type,
            subtitle,
            needle
        );
        candidates.push((rel_idx, subtitle, needle));
    }
    tracing::debug!(
        "auto-expand: subtitle path produced {} candidate(s)",
        candidates.len()
    );

    // NOTE: intentionally do NOT early-return on empty `candidates`.
    // The subtitle path can't find siblings whose titles either lack
    // a delimiter or use a single-word / generic-ordinal subtitle
    // (Egypt-hen, Second Season), but the episode-range fallback
    // below may still attribute overflow files to a relation by
    // episode count + title prefix. Let the fallback run.

    // First pass: for each media file, pick the candidate with the
    // LONGEST normalized needle that substring-matches the filename.
    // Longest-wins handles the Stardust ⊂ Stardust Crusaders case
    // without ever double-counting a file.
    let mut winner_by_file: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new(); // file_idx → (candidate_idx, needle_len)
    for (file_idx, name) in filenames.iter().enumerate() {
        if !is_media_filename(name) {
            continue;
        }
        let normalized = normalize_subtitle(name);
        let mut best: Option<(usize, usize)> = None; // (candidate_idx, needle_len)
        for (cand_idx, (_, _, needle)) in candidates.iter().enumerate() {
            if !normalized.contains(needle) {
                continue;
            }
            match best {
                Some((_, cur_len)) if cur_len >= needle.len() => {}
                _ => best = Some((cand_idx, needle.len())),
            }
        }
        if let Some(pick) = best {
            winner_by_file.insert(file_idx, pick);
        }
    }

    // Bucket files by winning candidate.
    let mut per_candidate: Vec<Vec<usize>> = vec![Vec::new(); candidates.len()];
    for (file_idx, (cand_idx, _)) in winner_by_file {
        per_candidate[cand_idx].push(file_idx);
    }
    for list in per_candidate.iter_mut() {
        list.sort_unstable();
    }

    // Materialize results. Drop candidates with no files and enforce
    // the per-sibling episode-count sanity cap.
    let mut out: Vec<SiblingMatch> = Vec::new();
    for (cand_idx, (rel_idx, subtitle, _)) in candidates.into_iter().enumerate() {
        let file_indices = std::mem::take(&mut per_candidate[cand_idx]);
        if file_indices.is_empty() {
            continue;
        }
        let rel: &RelatedEntry = &parent_detail.relations[rel_idx];
        if !within_episode_slack(file_indices.len(), rel.episodes.unwrap_or(0)) {
            continue;
        }
        out.push(SiblingMatch {
            anilist_id: rel.id,
            mal_id: rel.id_mal,
            title_romaji: rel.title_romaji.clone(),
            title_english: rel.title_english.clone(),
            title_native: rel.title_native.clone(),
            cover_url: rel.cover_url.clone(),
            format: rel.format.clone(),
            status: rel.status.clone(),
            episodes: rel.episodes,
            season_year: rel.season_year,
            matched_subtitle: subtitle,
            file_indices,
            // Populated in the offset pass below after fallback runs.
            episode_offset: 0,
        });
    }

    // Episode-range fallback: if the subtitle path came up empty
    // (e.g. a pack whose filenames carry plain episode numbers with
    // no arc name, so `trailing_subtitle_of` + substring matching
    // cannot find siblings), try to attribute episode-number overflow
    // (files whose ep > parent's episode count) to a sibling whose own
    // episode count makes the range fit.
    //
    // Only runs when `out.is_empty()` — i.e. the primary subtitle path
    // produced zero matches. If even one sibling was subtitle-matched,
    // we defer to that path and accept the known limitation that this
    // fallback can't supplement partial matches.
    if out.is_empty() {
        out.extend(detect_sibling_via_episode_range(filenames, parent_detail));
    }

    // Episode-offset pass: for every detected sibling (regardless of
    // code path), compute `episode_offset` from its own matched file
    // ep_nums. If the sibling's smallest parsed ep exceeds the parent's
    // episode count, the filenames are continuous-numbered across the
    // parent/sibling boundary and need `offset = parent_cap` applied
    // downstream (e.g. smol Owari: E14 is Owari S2 episode 1). If the
    // smallest ep is ≤ parent_cap, the filenames use arc-local numbering
    // and no offset is needed.
    let parent_cap = parent_detail.episodes.unwrap_or(0);
    if parent_cap > 0 {
        for m in out.iter_mut() {
            m.episode_offset =
                compute_sibling_episode_offset(&m.file_indices, filenames, parent_cap);
        }
    }

    out
}

/// Per-sibling offset detection. See [`SiblingMatch::episode_offset`].
///
/// Returns `min_ep - 1` when the sibling's smallest parsed episode
/// exceeds `parent_cap` — that makes the offset subtract out to a
/// local ep 1 regardless of how the pack aligns with AL's count.
/// Returns `0` when the sibling's episodes sit within the parent's
/// range (arc-local numbering, no offset needed).
///
/// In the common case where filenames use continuous numbering
/// starting at `parent_cap + 1`, `min_ep - 1 == parent_cap`, matching
/// the older behavior. When the pack's partition disagrees with AL
/// (e.g. a BD that splits a merged long-runtime episode into two
/// halves), `min_ep - 1 > parent_cap` and the offset correctly shifts
/// the sibling's first file to local ep 1 anyway.
fn compute_sibling_episode_offset(
    file_indices: &[usize],
    filenames: &[String],
    parent_cap: i32,
) -> i32 {
    if parent_cap <= 0 {
        return 0;
    }
    let min_ep = file_indices
        .iter()
        .filter_map(|&i| filenames.get(i))
        .filter(|n| is_media_filename(n))
        .filter_map(|n| media::parse_episode_number(&n.to_ascii_lowercase()))
        .map(|(_, ep)| ep)
        .min();
    match min_ep {
        Some(m) if m > parent_cap => m - 1,
        _ => 0,
    }
}

/// Layer 2 fallback: attribute files whose parsed episode number
/// exceeds `parent_cap` to one or more sibling relations via an
/// iterative sequential-packing algorithm. Each round picks the best
/// unclaimed candidate for the next contiguous slot
/// `[base+1..=base+sib_cap]` (starting at `base = parent_cap`) and
/// advances `base` by `sib_cap` after consuming that slot's files.
///
/// A candidate is eligible for a round if it
///
/// 1. passes the pack-candidate relation-type gate,
/// 2. is either a direct `SEQUEL` OR has a title that prefixes the
///    parent's title (catches continuations where AniList's listed
///    `SEQUEL` is semantically wrong — e.g. Owarimonogatari's listed
///    SEQUEL is Tsukimonogatari, but the same-title continuation is
///    Owarimonogatari Second Season),
/// 3. has a known episode count, and
/// 4. has at least `ceil(sib_cap * 0.75)` (floor 1) files from the
///    remaining overflow whose parsed `ep` falls into its expected
///    range. Partial fit is allowed — we don't demand that every
///    overflow file land inside the candidate's range, because real-
///    world absolute-numbered packs often bundle tail-end siblings
///    (e.g. a 1-episode sequel movie) that aren't in the grafted
///    relation pool. Files outside the chosen candidate's range
///    carry over to the next round.
///
/// Round winner is the scored candidate with the most files in its
/// expected range. Tiebreaker order: title-prefix-matched beats
/// non-prefix; smaller `sib_cap` beats larger (favors the tighter
/// fit). If the top two are tied on all criteria, the round bails
/// and the loop terminates — we'd rather return partial results
/// than guess. A first-round bail returns an empty `Vec`; a later-
/// round bail keeps results from previous rounds.
fn detect_sibling_via_episode_range(
    filenames: &[String],
    parent_detail: &AnimeDetail,
) -> Vec<SiblingMatch> {
    let parent_cap = parent_detail.episodes.unwrap_or(0);
    tracing::debug!(
        "auto-expand: fallback start parent_cap={} relations={}",
        parent_cap,
        parent_detail.relations.len()
    );
    if parent_cap <= 0 {
        tracing::debug!(
            "auto-expand: fallback bail reason=parent_cap<=0 (parent has no episode count)"
        );
        return Vec::new();
    }

    // Parse episodes per file (media files only).
    let mut overflow: Vec<(usize, i32)> = Vec::new();
    let mut media_count: usize = 0;
    let mut parsed_count: usize = 0;
    for (idx, name) in filenames.iter().enumerate() {
        if !is_media_filename(name) {
            continue;
        }
        media_count += 1;
        let Some((_, ep)) = media::parse_episode_number(&name.to_ascii_lowercase()) else {
            continue;
        };
        parsed_count += 1;
        if ep > parent_cap {
            overflow.push((idx, ep));
        }
    }
    tracing::debug!(
        "auto-expand: fallback overflow scan media_files={} parsed={} overflow_files={} parent_cap={}",
        media_count,
        parsed_count,
        overflow.len(),
        parent_cap
    );
    if overflow.is_empty() {
        tracing::debug!(
            "auto-expand: fallback bail reason=no-overflow-files (no parsed ep > parent_cap)"
        );
        return Vec::new();
    }
    let overflow_min = overflow.iter().map(|(_, e)| *e).min().unwrap();
    let overflow_max = overflow.iter().map(|(_, e)| *e).max().unwrap();
    let overflow_count = overflow.len();
    tracing::debug!(
        "auto-expand: fallback overflow range [{}..={}] count={}",
        overflow_min,
        overflow_max,
        overflow_count
    );

    // Parent title prefixes for continuation matching. Both english
    // and romaji forms are tried because AniList releases are
    // commonly titled in either.
    let parent_en = normalize_subtitle(&parent_detail.title_english);
    let parent_ro = normalize_subtitle(&parent_detail.title_romaji);
    let parent_prefixes: Vec<&str> = [parent_en.as_str(), parent_ro.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    tracing::debug!(
        "auto-expand: fallback parent prefixes en='{}' ro='{}'",
        parent_en,
        parent_ro
    );

    // Collect viable candidates ONCE (type/media/episode-count +
    // prefix-or-sequel gates). Per-round range fit is decided inside
    // the packing loop so each round can see the current `base`.
    struct RangeCandidate {
        rel_idx: usize,
        sib_cap: i32,
        title_prefix_matched: bool,
    }
    let mut candidates: Vec<RangeCandidate> = Vec::new();
    for (rel_idx, rel) in parent_detail.relations.iter().enumerate() {
        let rel_label = if !rel.title_english.is_empty() {
            rel.title_english.as_str()
        } else {
            rel.title_romaji.as_str()
        };
        if !rel.media_type.eq_ignore_ascii_case("ANIME") {
            tracing::debug!(
                "auto-expand: fallback skip rel='{}' reason=media_type={}",
                rel_label,
                rel.media_type
            );
            continue;
        }
        if !is_pack_candidate_relation(&rel.relation_type) {
            tracing::debug!(
                "auto-expand: fallback skip rel='{}' reason=relation_type={}",
                rel_label,
                rel.relation_type
            );
            continue;
        }
        let sib_cap = match rel.episodes {
            Some(n) if n > 0 => n,
            _ => {
                tracing::debug!(
                    "auto-expand: fallback skip rel='{}' reason=no-episode-count episodes={:?} relation_type={}",
                    rel_label,
                    rel.episodes,
                    rel.relation_type
                );
                continue;
            }
        };

        // Title-prefix test against both english and romaji forms of
        // the relation. A match requires the relation's normalized
        // title to *start with* a parent prefix AND be strictly longer
        // (exact equality would be a self-match, not a continuation).
        let rel_en = normalize_subtitle(&rel.title_english);
        let rel_ro = normalize_subtitle(&rel.title_romaji);
        let title_prefix_matched = parent_prefixes.iter().any(|p| {
            let p_len = p.len();
            (!rel_en.is_empty() && rel_en.len() > p_len && rel_en.starts_with(p))
                || (!rel_ro.is_empty() && rel_ro.len() > p_len && rel_ro.starts_with(p))
        });
        let is_sequel = rel.relation_type.eq_ignore_ascii_case("SEQUEL");
        if !title_prefix_matched && !is_sequel {
            tracing::debug!(
                "auto-expand: fallback skip rel='{}' reason=neither-sequel-nor-title-prefix relation_type={} rel_en='{}' rel_ro='{}'",
                rel_label,
                rel.relation_type,
                rel_en,
                rel_ro
            );
            continue;
        }

        tracing::debug!(
            "auto-expand: fallback candidate rel='{}' sib_cap={} title_prefix_matched={} is_sequel={}",
            rel_label,
            sib_cap,
            title_prefix_matched,
            is_sequel
        );
        candidates.push(RangeCandidate {
            rel_idx,
            sib_cap,
            title_prefix_matched,
        });
    }

    if candidates.is_empty() {
        tracing::debug!("auto-expand: fallback bail reason=zero-viable-candidates");
        return Vec::new();
    }

    // Sanity check: if the overflow is wildly larger than the sum
    // of all candidate capacities, the pack almost certainly doesn't
    // correspond to the sibling pool we see — e.g., an absolute-
    // numbered mega-franchise where most of the real siblings aren't
    // in our graph at all. Refuse to attribute a tiny fraction of
    // the overflow to one sibling in that case.
    let total_candidate_capacity: i32 = candidates.iter().map(|c| c.sib_cap).sum();
    if !within_episode_slack(overflow_count, total_candidate_capacity) {
        tracing::debug!(
            "auto-expand: fallback bail reason=overflow-exceeds-total-capacity overflow_count={} total_capacity={}",
            overflow_count,
            total_candidate_capacity
        );
        return Vec::new();
    }

    // Shared claim state for the filename-subtitle pre-pass and the
    // numeric packing loop below. Filename claims go in first and
    // the loop naturally skips anything they've already consumed.
    let mut results: Vec<SiblingMatch> = Vec::new();
    let mut claimed_rel_idxs: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut claimed_file_idxs: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // ── Filename-subtitle pre-pass ────────────────────────────────
    //
    // AL's `parent.episodes` can disagree with how a specific release
    // partitions its files. The canonical example is Owarimonogatari
    // on BD: AL reports 12 eps (the first aired episode was 48 min
    // and AL groups it as one) but the [smol] BD release splits the
    // 48-min ep 1 back into two ~24-min halves, so the pack has 13
    // Owarimonogatari files (S07E01..=E13) followed by 7 Owari S2
    // files (S07E14..=E20). Forward-aligned numeric packing anchored
    // at `parent_cap = 12` would mis-route S07E13 (Owari 1 ep 13)
    // into Owari S2 as "ep 1" and leave S07E20 (the real Owari S2
    // finale) hanging under the parent.
    //
    // The filenames themselves carry ground truth: `S07E13 -
    // Owarimonogatari (...)` vs `S07E14 - Owarimonogatari Second
    // Season (...)`. So before numeric packing runs, scan each
    // overflow file for a substring match against the parent's or a
    // candidate's normalized title. Longest-prefix wins (so "Owari…
    // Second Season" beats plain "Owari…"). Parent-matched files are
    // dropped from the overflow pool (they fall to parent by default);
    // candidate-matched files are pre-claimed to that candidate and
    // the candidate is marked consumed so numeric packing can't
    // duplicate-claim it.
    //
    // Needles shorter than 8 chars are rejected to avoid false
    // positives from short titles colliding with episode markers
    // or common filename tokens (e.g. "Show 2" matching "Show - 02",
    // or a 4-letter romaji title colliding with a group tag). 8 is
    // a heuristic floor: it keeps the substring match cheap while
    // still covering the vast majority of anime titles.
    //
    // Known tradeoff: short-title series (Bleach, Naruto, Gintama,
    // K-On!, One Piece) have normalized-title lengths below 8 chars
    // and bypass the filename-subtitle pre-pass entirely. Those
    // series fall through to the numeric packing path, which handles
    // them correctly via episode-range analysis — the filename
    // pre-pass is only load-bearing for long-title franchises that
    // share a numbering scheme across split sagas (Monogatari is
    // the motivating case). If you ever need to lower this floor,
    // add a collision check against known episode-marker shapes
    // first, or the Bleach/Naruto falsies will come roaring back.
    #[derive(Clone, Copy)]
    enum NeedleSource {
        Parent,
        Candidate(usize), // index into `candidates`
    }
    const MIN_FILENAME_NEEDLE_LEN: usize = 8;
    let mut needles: Vec<(NeedleSource, String)> = Vec::new();
    for p in [parent_en.as_str(), parent_ro.as_str()] {
        if p.len() >= MIN_FILENAME_NEEDLE_LEN && !needles.iter().any(|(_, n)| n == p) {
            needles.push((NeedleSource::Parent, p.to_string()));
        }
    }
    for (cand_idx, cand) in candidates.iter().enumerate() {
        let rel = &parent_detail.relations[cand.rel_idx];
        let en = normalize_subtitle(&rel.title_english);
        let ro = normalize_subtitle(&rel.title_romaji);
        for n in [en, ro] {
            if n.len() >= MIN_FILENAME_NEEDLE_LEN && !needles.iter().any(|(_, x)| x == &n) {
                needles.push((NeedleSource::Candidate(cand_idx), n));
            }
        }
    }
    tracing::debug!(
        "auto-expand: fallback filename-subtitle needles={}",
        needles.len()
    );

    let mut filename_claimed_by_cand: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    // Parent pre-claims are staged separately: in bare-number packs
    // every file matches parent's root title, so unconditionally
    // consuming them would strip the whole overflow and starve the
    // numeric packing loop. Only apply them if at least one sibling
    // was pre-claimed (i.e., the filenames actually distinguish arcs).
    let mut filename_claimed_parent: Vec<usize> = Vec::new();
    for (f_idx, ep) in overflow.iter() {
        let norm = normalize_subtitle(&filenames[*f_idx]);
        let mut best: Option<(NeedleSource, usize)> = None;
        for (source, needle) in &needles {
            if !norm.contains(needle) {
                continue;
            }
            let len = needle.len();
            match best {
                Some((_, cur)) if cur >= len => {}
                _ => best = Some((*source, len)),
            }
        }
        match best {
            Some((NeedleSource::Parent, len)) => {
                filename_claimed_parent.push(*f_idx);
                tracing::debug!(
                    "auto-expand: fallback filename-subtitle file_idx={} ep={} → parent (needle_len={})",
                    f_idx,
                    ep,
                    len
                );
            }
            Some((NeedleSource::Candidate(cand_idx), len)) => {
                filename_claimed_by_cand
                    .entry(cand_idx)
                    .or_default()
                    .push(*f_idx);
                tracing::debug!(
                    "auto-expand: fallback filename-subtitle file_idx={} ep={} → candidate[{}] (needle_len={})",
                    f_idx,
                    ep,
                    cand_idx,
                    len
                );
            }
            None => {}
        }
    }

    let have_sibling_evidence = !filename_claimed_by_cand.is_empty();
    if have_sibling_evidence {
        for f_idx in &filename_claimed_parent {
            claimed_file_idxs.insert(*f_idx);
        }
        tracing::debug!(
            "auto-expand: fallback filename-subtitle applied {} parent pre-claims",
            filename_claimed_parent.len()
        );
    } else if !filename_claimed_parent.is_empty() {
        tracing::debug!(
            "auto-expand: fallback filename-subtitle discarded {} parent pre-claims (no sibling evidence)",
            filename_claimed_parent.len()
        );
    }

    for (cand_idx, mut file_indices) in filename_claimed_by_cand.into_iter() {
        let cand = &candidates[cand_idx];
        let rel = &parent_detail.relations[cand.rel_idx];
        if !within_episode_slack(file_indices.len(), cand.sib_cap) {
            tracing::debug!(
                "auto-expand: fallback filename-subtitle skip cand[{}] reason=slack-cap files={} sib_cap={}",
                cand_idx,
                file_indices.len(),
                cand.sib_cap
            );
            continue;
        }
        file_indices.sort_unstable();
        claimed_rel_idxs.insert(cand.rel_idx);
        for f in &file_indices {
            claimed_file_idxs.insert(*f);
        }
        let label = if !rel.title_english.is_empty() {
            rel.title_english.as_str()
        } else {
            rel.title_romaji.as_str()
        };
        tracing::debug!(
            "auto-expand: fallback filename-subtitle emitted sibling rel='{}' id={} files={}",
            label,
            rel.id,
            file_indices.len()
        );
        results.push(SiblingMatch {
            anilist_id: rel.id,
            mal_id: rel.id_mal,
            title_romaji: rel.title_romaji.clone(),
            title_english: rel.title_english.clone(),
            title_native: rel.title_native.clone(),
            cover_url: rel.cover_url.clone(),
            format: rel.format.clone(),
            status: rel.status.clone(),
            episodes: rel.episodes,
            season_year: rel.season_year,
            matched_subtitle: "episode-range fallback (filename subtitle match)".to_string(),
            file_indices,
            episode_offset: 0, // populated by the offset pass in the caller
        });
    }

    // ── Iterative sequential packing ──────────────────────────────
    let mut base: i32 = parent_cap;

    loop {
        // Short-circuit when there are no more overflow files beyond
        // the current base to attribute.
        let remaining_files: Vec<(usize, i32)> = overflow
            .iter()
            .filter(|(f_idx, ep)| !claimed_file_idxs.contains(f_idx) && *ep > base)
            .copied()
            .collect();
        if remaining_files.is_empty() {
            break;
        }

        // Score every unclaimed candidate for this round.
        struct Scored {
            rel_idx: usize,
            sib_cap: i32,
            file_indices: Vec<usize>,
            title_prefix_matched: bool,
        }
        let mut scored: Vec<Scored> = Vec::new();
        for cand in &candidates {
            if claimed_rel_idxs.contains(&cand.rel_idx) {
                continue;
            }
            let expected_min = base + 1;
            let expected_max = base + cand.sib_cap;
            let in_range: Vec<usize> = remaining_files
                .iter()
                .filter(|(_, ep)| *ep >= expected_min && *ep <= expected_max)
                .map(|(idx, _)| *idx)
                .collect();
            // Partial-fit threshold: at least ceil(sib_cap * 0.75)
            // files, floor 1. Keeps 1-episode movies accept-able
            // without letting a 12-ep sibling win on 1/12 files.
            let threshold = ((cand.sib_cap as f32) * 0.75).ceil() as i32;
            let threshold = threshold.max(1) as usize;
            if in_range.len() < threshold {
                continue;
            }
            scored.push(Scored {
                rel_idx: cand.rel_idx,
                sib_cap: cand.sib_cap,
                file_indices: in_range,
                title_prefix_matched: cand.title_prefix_matched,
            });
        }

        if scored.is_empty() {
            tracing::debug!(
                "auto-expand: fallback packing stop reason=no-viable-candidate-for-round base={}",
                base
            );
            break;
        }

        // Sort by: file_count DESC, title_prefix DESC, sib_cap ASC.
        // Then detect ambiguity if the top two are fully tied.
        scored.sort_by(|a, b| {
            b.file_indices
                .len()
                .cmp(&a.file_indices.len())
                .then(b.title_prefix_matched.cmp(&a.title_prefix_matched))
                .then(a.sib_cap.cmp(&b.sib_cap))
        });

        if scored.len() >= 2 {
            let top = &scored[0];
            let next = &scored[1];
            if top.file_indices.len() == next.file_indices.len()
                && top.title_prefix_matched == next.title_prefix_matched
                && top.sib_cap == next.sib_cap
            {
                tracing::debug!(
                    "auto-expand: fallback packing stop reason=round-ambiguous base={} candidates_tied={}",
                    base,
                    scored.len()
                );
                break;
            }
        }

        let winner = scored.into_iter().next().unwrap();
        claimed_rel_idxs.insert(winner.rel_idx);
        for f in &winner.file_indices {
            claimed_file_idxs.insert(*f);
        }

        let rel = &parent_detail.relations[winner.rel_idx];
        let chosen_label = if !rel.title_english.is_empty() {
            rel.title_english.as_str()
        } else {
            rel.title_romaji.as_str()
        };
        tracing::debug!(
            "auto-expand: fallback packing round picked rel='{}' anilist_id={} sib_cap={} files={} base_before={} base_after={}",
            chosen_label,
            rel.id,
            winner.sib_cap,
            winner.file_indices.len(),
            base,
            base + winner.sib_cap
        );

        let mut file_indices = winner.file_indices.clone();
        file_indices.sort_unstable();
        results.push(SiblingMatch {
            anilist_id: rel.id,
            mal_id: rel.id_mal,
            title_romaji: rel.title_romaji.clone(),
            title_english: rel.title_english.clone(),
            title_native: rel.title_native.clone(),
            cover_url: rel.cover_url.clone(),
            format: rel.format.clone(),
            status: rel.status.clone(),
            episodes: rel.episodes,
            season_year: rel.season_year,
            matched_subtitle: format!(
                "episode-range fallback ({}..={})",
                base + 1,
                base + winner.sib_cap
            ),
            file_indices,
            episode_offset: 0, // populated by the offset pass in the caller
        });
        base += winner.sib_cap;
    }

    tracing::debug!(
        "auto-expand: fallback packing done siblings={} remaining_overflow_unattributed={}",
        results.len(),
        overflow_count - claimed_file_idxs.len()
    );
    results
}

// Lowercase ASCII-alphanumeric chars and collapse non-alphanumeric
// runs to single spaces. Used to make subtitle-vs-filename comparisons
// robust to punctuation differences like "JoJo's" vs "JoJos",
// "Stardust-Crusaders" vs "Stardust Crusaders", or brackets.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::anilist::AnimeDetail;

    fn detail_with_titles(english: &str, romaji: &str) -> AnimeDetail {
        AnimeDetail {
            id: 1,
            id_mal: None,
            title_romaji: romaji.to_string(),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "MOVIE".to_string(),
            status: String::new(),
            status_display: String::new(),
            episodes: Some(1),
            duration: None,
            season: String::new(),
            season_year: None,
            end_year: None,
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn related(
        id: i64,
        english: &str,
        romaji: &str,
        relation_type: &str,
        episodes: Option<i32>,
    ) -> RelatedEntry {
        RelatedEntry {
            id,
            id_mal: None,
            title_romaji: romaji.to_string(),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes,
            relation_type: relation_type.to_string(),
            season_year: None,
            media_type: "ANIME".to_string(),
        }
    }

    #[test]
    fn detect_siblings_finds_named_seasons_in_jojo_pack() {
        // Parent: JoJo S1 (franchise root, no subtitle of its own).
        // Pack contains files for S1 (no subtitle), S3 Stardust
        // Crusaders, and S4 Diamond is Unbreakable. Detection should
        // return two sibling matches (Stardust + Diamond) with only
        // their own files; S1 files stay unclaimed.
        let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        parent.id = 14719; // AL id
        parent.episodes = Some(26);
        parent.relations = vec![
            related(
                20800,
                "JoJo's Bizarre Adventure: Stardust Crusaders",
                "JoJo no Kimyou na Bouken: Stardust Crusaders",
                "SEQUEL",
                Some(24),
            ),
            related(
                31292,
                "JoJo's Bizarre Adventure: Diamond is Unbreakable",
                "JoJo no Kimyou na Bouken: Diamond wa Kudakenai",
                "SEQUEL",
                Some(39),
            ),
        ];

        let files: Vec<String> = vec![
            // S1 files (unclaimed)
            "[Group] JoJo no Kimyou na Bouken - 01.mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - 02.mkv".to_string(),
            // Stardust Crusaders (24 eps, we include just 3 for brevity)
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01.mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 02.mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 03.mkv".to_string(),
            // Diamond is Unbreakable
            "[Group] JoJo no Kimyou na Bouken - Diamond is Unbreakable - 01.mkv".to_string(),
            "[Group] JoJo no Kimyou na Bouken - Diamond is Unbreakable - 02.mkv".to_string(),
        ];

        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 2, "expected Stardust + Diamond matches");

        let stardust = siblings
            .iter()
            .find(|s| s.anilist_id == 20800)
            .expect("stardust sibling present");
        assert_eq!(stardust.file_indices, vec![2, 3, 4]);
        assert!(
            stardust
                .matched_subtitle
                .to_lowercase()
                .contains("stardust"),
            "matched_subtitle should reference Stardust, got {:?}",
            stardust.matched_subtitle
        );

        let diamond = siblings
            .iter()
            .find(|s| s.anilist_id == 31292)
            .expect("diamond sibling present");
        assert_eq!(diamond.file_indices, vec![5, 6]);
    }

    #[test]
    fn detect_siblings_returns_empty_for_jikan_sourced_detail() {
        // Provenance gate: Jikan-sourced details have id < 0. Even
        // if relations look plausible, we must not run sibling
        // detection against them — MAL splits sagas AL merges, which
        // would duplicate library rows.
        let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "JoJo no Kimyou na Bouken");
        parent.id = -1; // Jikan sentinel
        parent.relations = vec![related(
            -20800,
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "",
            "SEQUEL",
            Some(24),
        )];
        let files: Vec<String> =
            vec!["[Group] JoJo no Kimyou na Bouken - Stardust Crusaders - 01.mkv".to_string()];
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_resolves_overlap_by_longest_subtitle() {
        // Two siblings whose subtitles form a prefix relationship. A
        // filename containing the longer subtitle matches both
        // normalized needles, but the longer one must win — otherwise
        // we'd double-count the file.
        let mut parent = detail_with_titles("Franchise", "Franchise");
        parent.id = 100;
        parent.relations = vec![
            related(201, "Franchise: Alpha", "", "SEQUEL", Some(12)),
            related(202, "Franchise: Alpha Prime", "", "SEQUEL", Some(12)),
        ];
        let files: Vec<String> = vec![
            "[Group] Franchise - Alpha Prime - 01.mkv".to_string(),
            "[Group] Franchise - Alpha Prime - 02.mkv".to_string(),
        ];
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0].anilist_id, 202);
        assert_eq!(siblings[0].file_indices, vec![0, 1]);
    }

    #[test]
    fn detect_siblings_skips_relations_without_own_subtitle() {
        // "Naruto Shippuden" has no trailing delimiter so
        // trailing_subtitle_of returns None and the sibling gets
        // silently dropped. This is intentional — without a
        // subtitle we can't safely narrow a filename list, so
        // conservative over-skipping is the right call.
        let mut parent = detail_with_titles("Naruto", "Naruto");
        parent.id = 20;
        parent.relations = vec![related(1735, "Naruto Shippuden", "", "SEQUEL", Some(500))];
        let files: Vec<String> = vec!["[Group] Naruto Shippuden - 01.mkv".to_string()];
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_rejects_episode_count_overshoot() {
        // A sibling with episodes=12 whose subtitle accidentally
        // matches 50 files in the pack. The episode-count cap
        // (×1.5 + 2 = 20) fires and drops the sibling entirely
        // rather than emitting a wildly-wrong routing.
        let mut parent = detail_with_titles("Franchise", "Franchise");
        parent.id = 100;
        parent.relations = vec![related(
            201,
            "Franchise: Alpha Beta",
            "",
            "SEQUEL",
            Some(12),
        )];
        let files: Vec<String> = (1..=50)
            .map(|i| format!("[Group] Franchise - Alpha Beta - {:02}.mkv", i))
            .collect();
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_filters_out_source_material_relations() {
        // ADAPTATION / SOURCE / COMPILATION / CONTAINS relations
        // point at manga / LN / book entries that will never appear
        // in an anime torrent. Even if one happened to share a
        // substring with a filename, the relation-type gate must
        // drop it before we waste cycles on string matching.
        let mut parent = detail_with_titles("JoJo's Bizarre Adventure", "");
        parent.id = 14719;
        parent.relations = vec![related(
            2,
            "JoJo's Bizarre Adventure: Stardust Crusaders",
            "",
            "SOURCE",
            Some(1),
        )];
        let files: Vec<String> = vec!["[Group] JoJo - Stardust Crusaders - 01.mkv".to_string()];
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_ignores_non_anime_media_types() {
        // AL returns the parent manga via a relation edge with
        // media_type="MANGA". Never an anime torrent candidate.
        let mut parent = detail_with_titles("Show", "");
        parent.id = 10;
        let mut manga_rel = related(5, "Show: Spinoff Arc", "", "SIDE_STORY", Some(10));
        manga_rel.media_type = "MANGA".to_string();
        parent.relations = vec![manga_rel];
        let files: Vec<String> = vec!["[Group] Show - Spinoff Arc - 01.mkv".to_string()];
        assert!(detect_sibling_entries_in_pack(&files, &parent).is_empty());
    }

    #[test]
    fn detect_siblings_passes_through_spin_off_and_summary_relations() {
        // Niche relation types (SPIN_OFF, SUMMARY, CHARACTER,
        // ALTERNATIVE) are included in the filter — the subtitle
        // match and episode-count cap do the downstream filtering.
        let mut parent = detail_with_titles("Show", "");
        parent.id = 10;
        parent.relations = vec![
            related(11, "Show: Recap Arc", "", "SUMMARY", Some(4)),
            related(12, "Show: Extra Chapter", "", "SPIN_OFF", Some(6)),
        ];
        let files: Vec<String> = vec![
            "[Group] Show - Recap Arc - 01.mkv".to_string(),
            "[Group] Show - Recap Arc - 02.mkv".to_string(),
            "[Group] Show - Extra Chapter - 01.mkv".to_string(),
        ];
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 2);
        let recap = siblings
            .iter()
            .find(|s| s.anilist_id == 11)
            .expect("recap sibling present");
        assert_eq!(recap.file_indices, vec![0, 1]);
        let extra = siblings
            .iter()
            .find(|s| s.anilist_id == 12)
            .expect("extra sibling present");
        assert_eq!(extra.file_indices, vec![2]);
    }

    #[test]
    fn detect_siblings_ignores_non_media_files_in_match_set() {
        // Subtitles, NFOs, samples etc. must not count toward the
        // episode cap or get routed. Only .mkv/.mp4/... files pass
        // through is_media_filename.
        let mut parent = detail_with_titles("Show", "");
        parent.id = 10;
        parent.relations = vec![related(11, "Show: Alpha Beta", "", "SEQUEL", Some(12))];
        let files: Vec<String> = vec![
            "[Group] Show - Alpha Beta - 01.mkv".to_string(),
            "[Group] Show - Alpha Beta - 01.srt".to_string(),
            "[Group] Show - Alpha Beta - readme.nfo".to_string(),
        ];
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1);
        // Only the .mkv file routes — the .srt and .nfo are filtered
        // out by is_media_filename before they can inflate the match
        // set.
        assert_eq!(siblings[0].file_indices, vec![0]);
    }

    // ── Layer 2: episode-range fallback ────────────────────────────

    #[test]
    fn detect_siblings_fallback_catches_bare_number_pack_single_word_arc() {
        // 48-file continuation pack using bare space-delimited episode
        // numbers followed by a quality bracket (no `S01E01`, no
        // `- 25`), where the sibling's trailing subtitle is a single
        // word and thus rejected by `trailing_subtitle_of`'s ≥2-token
        // rule. The subtitle path produces zero matches and files
        // 25-48 must come through the episode-range fallback, which
        // attributes them to the sibling with
        // `episode_offset = parent_cap`.
        //
        // Filenames here are synthetic token strings — the only thing
        // the test cares about is the bare-digit + quality-bracket
        // shape, since that's what `parse_episode_number`'s new
        // RE_BARE_NUM_BRACKET branch keys on.
        let mut parent = detail_with_titles("Parent Show", "Parent Show Romaji");
        parent.id = 20474;
        parent.episodes = Some(24);
        parent.relations = vec![related(
            20799,
            "Parent Show - Coda",
            "Parent Show - Coda",
            "SEQUEL",
            Some(24),
        )];

        let mut files: Vec<String> = Vec::new();
        for n in 1..=48 {
            files.push(format!(
                "fixture-parent-show {:02} (bd-1080p) [hash].mkv",
                n
            ));
        }

        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1, "fallback should find one sibling");
        let s = &siblings[0];
        assert_eq!(s.anilist_id, 20799);
        // Sibling claims files 24..48 (indices 24..=47 → eps 25..=48).
        assert_eq!(s.file_indices.len(), 24);
        assert_eq!(*s.file_indices.first().unwrap(), 24);
        assert_eq!(*s.file_indices.last().unwrap(), 47);
        assert!(s.matched_subtitle.starts_with("episode-range fallback"));
        // Absolute numbering → offset = parent cap (24).
        assert_eq!(s.episode_offset, 24);
    }

    #[test]
    fn detect_siblings_fallback_rejects_ambiguous_two_sequels() {
        // Parent with two SEQUEL relations that both fit the overflow
        // range ambiguously. Neither is title-prefix matched, so the
        // tiebreaker doesn't save us → bail, fallback returns nothing.
        let mut parent = detail_with_titles("Parent Show", "");
        parent.id = 1;
        parent.episodes = Some(12);
        parent.relations = vec![
            related(2, "Unrelated Sequel One", "", "SEQUEL", Some(12)),
            related(3, "Unrelated Sequel Two", "", "SEQUEL", Some(12)),
        ];
        let mut files: Vec<String> = Vec::new();
        for n in 1..=24 {
            files.push(format!("[Group] Parent Show - {:02}.mkv", n));
        }
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert!(siblings.is_empty(), "ambiguous sequels must bail");
    }

    #[test]
    fn detect_siblings_fallback_title_prefix_beats_strict_sequel() {
        // Owarimonogatari scenario: direct AniList SEQUEL is
        // Tsukimonogatari (not a continuation of the same title),
        // but the actual same-title continuation is Owarimonogatari
        // Second Season. Range-fit alone can't distinguish if both
        // candidates pass, so title-prefix wins as the tiebreaker.
        //
        // Here we give Tsuki an incompatible ep count (can't fit the
        // overflow) so it's rejected by range-fit first, and Owari S2
        // is the only survivor — validating the primary path.
        let mut parent = detail_with_titles("Owarimonogatari", "Owarimonogatari");
        parent.id = 21320;
        parent.episodes = Some(13);
        parent.relations = vec![
            // Direct SEQUEL relation, wrong continuation — only 4 eps
            // so it cannot fit a 7-file overflow.
            related(
                20787,
                "Tsukimonogatari",
                "Tsukimonogatari",
                "SEQUEL",
                Some(4),
            ),
            // Same-title continuation; AniList may type this as a
            // SIDE_STORY so we must admit it via title-prefix.
            related(
                21860,
                "Owarimonogatari Second Season",
                "Owarimonogatari Second Season",
                "SIDE_STORY",
                Some(7),
            ),
        ];
        let mut files: Vec<String> = Vec::new();
        for n in 1..=20 {
            files.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari.mkv",
                n
            ));
        }

        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1);
        let s = &siblings[0];
        assert_eq!(
            s.anilist_id, 21860,
            "must pick Owari S2, not Tsukimonogatari"
        );
        assert_eq!(s.file_indices.len(), 7);
        assert_eq!(
            s.episode_offset, 13,
            "absolute numbering → offset = parent cap"
        );
    }

    #[test]
    fn detect_siblings_fallback_skips_when_no_overflow() {
        // 12-ep parent, 12 files numbered 01..12 — nothing exceeds the
        // parent cap, so the fallback must not synthesize siblings.
        let mut parent = detail_with_titles("Parent Show", "");
        parent.id = 1;
        parent.episodes = Some(12);
        parent.relations = vec![related(
            2,
            "Parent Show Second Season",
            "",
            "SEQUEL",
            Some(12),
        )];
        let mut files: Vec<String> = Vec::new();
        for n in 1..=12 {
            files.push(format!("[Group] Parent Show - {:02}.mkv", n));
        }
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert!(siblings.is_empty());
    }

    #[test]
    fn detect_siblings_fallback_skipped_when_parent_episodes_unknown() {
        // Airing / unknown-length parent (episodes=None): we can't
        // safely attribute overflow. Fallback bails.
        let mut parent = detail_with_titles("Parent Show", "");
        parent.id = 1;
        parent.episodes = None;
        parent.relations = vec![related(
            2,
            "Parent Show Second Season",
            "",
            "SEQUEL",
            Some(12),
        )];
        let files: Vec<String> = (1..=12)
            .map(|n| format!("[Group] Parent Show - {:02}.mkv", n))
            .collect();
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert!(siblings.is_empty());
    }

    #[test]
    fn detect_siblings_fallback_skipped_when_subtitle_path_found_something() {
        // Subtitle path hit at least one sibling → fallback is
        // suppressed. Parent has relation with a usable 2-token
        // subtitle AND files matching it, so subtitle path produces
        // results. Fallback won't run even if overflow files exist
        // (known limitation — fallback doesn't supplement partial
        // subtitle matches).
        let mut parent = detail_with_titles("Parent Show", "");
        parent.id = 1;
        parent.episodes = Some(12);
        parent.relations = vec![
            related(2, "Parent Show: Alpha Beta", "", "SEQUEL", Some(12)),
            related(3, "Parent Show Third Season", "", "SEQUEL", Some(12)),
        ];
        let files: Vec<String> = vec![
            "[Group] Parent Show - Alpha Beta - 01.mkv".to_string(),
            "[Group] Parent Show - Alpha Beta - 02.mkv".to_string(),
            // These would be overflow for a 12-ep parent but subtitle
            // path already produced matches, so fallback is suppressed.
            "[Group] Parent Show - 25.mkv".to_string(),
            "[Group] Parent Show - 26.mkv".to_string(),
        ];
        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0].anilist_id, 2);
    }

    #[test]
    fn detect_siblings_fallback_handles_absolute_numbered_smol_owari() {
        // Real-world: [smol] Monogatari batch uses continuous
        // absolute numbering (E13 Owarimonogatari, E14 Owarimonogatari
        // Second Season, ...). Subtitle detection can't fire —
        // "Owarimonogatari Second Season" has no ": " / " - "
        // delimiter AND its trailing portion is a generic-ordinal
        // phrase anyway — so the episode-range fallback is the only
        // path that reaches this release. It picks Owari S2 via
        // title-prefix matching and the per-sibling offset pass
        // applies offset=13 so post_processing renames E14..E20 to
        // E01..E07 of Owari S2.
        let mut parent = detail_with_titles("Owarimonogatari", "Owarimonogatari");
        parent.id = 21320;
        parent.episodes = Some(13);
        parent.relations = vec![related(
            21860,
            "Owarimonogatari Second Season",
            "Owarimonogatari Second Season",
            "SIDE_STORY",
            Some(7),
        )];
        let mut files: Vec<String> = Vec::new();
        for n in 1..=13 {
            files.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p).mkv",
                n
            ));
        }
        for n in 14..=20 {
            files.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p).mkv",
                n
            ));
        }

        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1, "fallback should find Owari S2");
        let s = &siblings[0];
        assert_eq!(s.anilist_id, 21860);
        // Overflow = files with E14..E20 (indices 13..=19).
        assert_eq!(s.file_indices.len(), 7);
        assert_eq!(*s.file_indices.first().unwrap(), 13);
        assert_eq!(*s.file_indices.last().unwrap(), 19);
        assert!(s.matched_subtitle.starts_with("episode-range fallback"));
        assert_eq!(
            s.episode_offset, 13,
            "absolute numbering → offset = parent cap"
        );
    }

    #[test]
    fn detect_siblings_fallback_partial_fit_multi_sibling_owari_with_zoku() {
        // Real-world progression of the smol Owari case: the pack has
        // 20 files (S07E01..=E20) but parent_cap = 12 (some AL data
        // sources report Owarimonogatari with 12 ep count), so
        // overflow = 8 files (eps 13..=20). The only graph-visible
        // sibling is Owarimonogatari Second Season (7 eps). The 8th
        // overflow file is Zoku Owarimonogatari, a 1-episode movie
        // that either isn't grafted or has no episodes count.
        //
        // Expected: packing loop picks Owari S2 for 7 of 8 overflow
        // files (partial fit, threshold = ceil(7*0.75) = 6), ep 20
        // falls out as unattributed. Emitting one partial-fit
        // sibling is better than bailing entirely, because the 7
        // files that DO fit cleanly are definitively Owari S2.
        let mut parent = detail_with_titles("Owarimonogatari", "Owarimonogatari");
        parent.id = 21320;
        parent.episodes = Some(12);
        parent.relations = vec![
            // Direct SEQUEL is Tsukimonogatari — fits the first 4
            // overflow eps but not title-prefixed.
            related(
                20787,
                "Tsukimonogatari",
                "Tsukimonogatari",
                "SEQUEL",
                Some(4),
            ),
            // Same-title continuation, transitively grafted as
            // SIDE_STORY. Owari S2 takes precedence because it's
            // title-prefixed AND covers more of the overflow in
            // round 1.
            related(
                21860,
                "Owarimonogatari Second Season",
                "Owarimonogatari Second Season",
                "SIDE_STORY",
                Some(7),
            ),
        ];
        let mut files: Vec<String> = Vec::new();
        for n in 1..=12 {
            files.push(format!(
                "fixture-monogatari-s07e{:02}-owarimonogatari-bd-1080p.mkv",
                n
            ));
        }
        for n in 13..=19 {
            files.push(format!(
                "fixture-monogatari-s07e{:02}-owarimonogatari-second-season-bd-1080p.mkv",
                n
            ));
        }
        files.push("fixture-monogatari-s07e20-zoku-owarimonogatari-bd-1080p.mkv".to_string());

        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1, "must emit Owari S2 as partial fit");
        let s = &siblings[0];
        assert_eq!(s.anilist_id, 21860);
        assert_eq!(
            s.file_indices.len(),
            7,
            "seven files in [13..=19] must be claimed"
        );
        assert_eq!(*s.file_indices.first().unwrap(), 12);
        assert_eq!(*s.file_indices.last().unwrap(), 18);
        // File index 19 (ep 20, Zoku) must NOT be claimed — it falls
        // outside Owari S2's range and no other candidate can cover it.
        assert!(
            !s.file_indices.contains(&19),
            "ep 20 Zoku file must remain unattributed"
        );
        assert_eq!(
            s.episode_offset, 12,
            "absolute numbering → offset = parent cap"
        );
    }

    #[test]
    fn detect_siblings_fallback_filename_subtitle_corrects_bd_split_first_ep() {
        // Real-world case from the live [smol] Owarimonogatari grab
        // (reported 2026-04-15): the BD release splits the 48-min
        // first aired episode back into two ~24-min halves, so the
        // pack has 13 Owari 1 files (S07E01..=E13) followed by 7
        // Owari 2 files (S07E14..=E20). But AniList reports the
        // parent as 12 eps (it groups the merged broadcast ep 1 as
        // one). Forward-aligned numeric packing anchored at
        // parent_cap=12 would misroute S07E13 (Owari 1's last ep) to
        // Owari S2 as "ep 1" and leave S07E20 (the real Owari S2
        // finale) hanging under the parent.
        //
        // The filename subtitle pre-pass fixes this: S07E13's file-
        // name only contains "Owarimonogatari", matching the parent
        // title, so it's parent-pre-claimed. S07E14..=E20 contain
        // "Owarimonogatari Second Season", matching the sibling
        // title (longer → wins), so those 7 files are sibling-pre-
        // claimed. The episode offset comes out to 13 (min_ep - 1),
        // so post-processing renames S07E14..=E20 to Owari S2
        // E01..=E07 correctly.
        //
        // Filenames are taken directly from the user's grab; they
        // correspond to a specific real release of a real group.
        let mut parent = detail_with_titles("Owarimonogatari", "Owarimonogatari");
        parent.id = 21262;
        parent.episodes = Some(12); // AL undercount — BD has 13 files for parent
        parent.relations = vec![
            related(
                20787,
                "Tsukimonogatari",
                "Tsukimonogatari",
                "SEQUEL",
                Some(4),
            ),
            related(
                21745,
                "Owarimonogatari Second Season",
                "Owarimonogatari Second Season",
                "SEQUEL",
                Some(7),
            ),
        ];
        let mut files: Vec<String> = Vec::new();
        for n in 1..=13 {
            files.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari (BD 1080p HEVC Opus) [DEADBEEF].mkv",
                n
            ));
        }
        for n in 14..=20 {
            files.push(format!(
                "[smol] Monogatari - S07E{:02} - Owarimonogatari Second Season (Ge) (BD 1080p HEVC Opus) [DEADBEEF].mkv",
                n
            ));
        }

        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(
            siblings.len(),
            1,
            "filename subtitle pre-pass should identify Owari S2"
        );
        let s = &siblings[0];
        assert_eq!(s.anilist_id, 21745);
        assert_eq!(
            s.file_indices.len(),
            7,
            "Owari S2 should claim exactly the 7 S07E14..=E20 files"
        );
        // File indices 13..=19 correspond to S07E14..=E20 (files
        // vec is 0-indexed).
        assert_eq!(*s.file_indices.first().unwrap(), 13);
        assert_eq!(*s.file_indices.last().unwrap(), 19);
        // Critically: file index 12 (S07E13) must NOT be claimed —
        // that's Owari 1's last ep, and misrouting it was the bug.
        assert!(
            !s.file_indices.contains(&12),
            "S07E13 (Owari 1 ep 13) must stay with parent"
        );
        // Offset: min_ep = 14, so offset = 13 (not parent_cap = 12).
        // S07E14 → 14 - 13 = Owari S2 ep 1. Correct.
        assert_eq!(
            s.episode_offset, 13,
            "offset must be min_ep - 1 = 13 so Owari S2 starts at local ep 1"
        );
    }

    // ── transitive_relation_graft ────────────────────────────────────

    /// Build an `AnimeDetail` with a specific id, titles, episode
    /// count, and a pre-populated relation list. Used by the
    /// transitive-walk tests to construct the neighbor details that
    /// the graft helper walks into.
    fn detail_with_relations(
        id: i64,
        english: &str,
        romaji: &str,
        episodes: Option<i32>,
        relations: Vec<RelatedEntry>,
    ) -> AnimeDetail {
        let mut d = detail_with_titles(english, romaji);
        d.id = id;
        d.episodes = episodes;
        d.relations = relations;
        d.format = "TV".to_string();
        d
    }

    #[test]
    fn transitive_graft_pulls_in_second_hop_when_direct_relations_are_missing_edge() {
        // Parent has ONE direct PREQUEL neighbor. That neighbor's own
        // relations include a sibling that is NOT in the parent's
        // direct relations — the missing-edge case the walk exists to
        // fix. Graft should surface the missing sibling.
        let parent_id = 100;
        let neighbor_id = 200;
        let missing_sibling_id = 300;

        let parent = detail_with_relations(
            parent_id,
            "Parent Show",
            "Parent Show",
            Some(12),
            vec![related(
                neighbor_id,
                "Neighbor",
                "Neighbor",
                "PREQUEL",
                Some(26),
            )],
        );
        let neighbor = detail_with_relations(
            neighbor_id,
            "Neighbor",
            "Neighbor",
            Some(26),
            vec![
                // Back-edge to parent — must be deduped out.
                related(parent_id, "Parent Show", "Parent Show", "SEQUEL", Some(12)),
                // The sibling we want to surface.
                related(
                    missing_sibling_id,
                    "Parent Show Continuation",
                    "Parent Show Continuation",
                    "SEQUEL",
                    Some(7),
                ),
            ],
        );
        let mut neighbors = std::collections::HashMap::new();
        neighbors.insert(neighbor_id, neighbor);

        let graft = transitive_relation_graft(&parent, &neighbors);
        assert_eq!(graft.len(), 1, "back-edge to parent must be deduped");
        assert_eq!(graft[0].id, missing_sibling_id);
    }

    #[test]
    fn transitive_graft_skips_non_walkable_relation_types() {
        // Parent has an ADAPTATION neighbor (manga). The walk must
        // NOT fetch into it even if we seed the map — is_pack_candidate
        // already blocks ADAPTATION as a direct sibling, and
        // is_transitive_walk_source must also block it as a walk
        // source.
        let parent = detail_with_relations(
            1,
            "Parent",
            "Parent",
            Some(12),
            vec![related(2, "Manga", "Manga", "ADAPTATION", None)],
        );
        // Seed neighbor map anyway — graft should ignore it because
        // the direct relation's type isn't walkable.
        let neighbor = detail_with_relations(
            2,
            "Manga",
            "Manga",
            None,
            vec![related(
                3,
                "Something Else",
                "Something Else",
                "SEQUEL",
                Some(13),
            )],
        );
        let mut neighbors = std::collections::HashMap::new();
        neighbors.insert(2, neighbor);

        let graft = transitive_relation_graft(&parent, &neighbors);
        assert!(
            graft.is_empty(),
            "ADAPTATION direct relation must not be walked"
        );
    }

    #[test]
    fn transitive_graft_dedupes_against_direct_relations() {
        // Parent already has a direct relation to id=5. Its neighbor
        // (reachable via PREQUEL) also lists id=5 as a sibling. The
        // graft must NOT return id=5 again — it's already in the
        // parent's direct list.
        let parent = detail_with_relations(
            1,
            "Parent",
            "Parent",
            Some(12),
            vec![
                related(2, "Neighbor", "Neighbor", "PREQUEL", Some(26)),
                related(5, "Already Direct", "Already Direct", "SEQUEL", Some(7)),
            ],
        );
        let neighbor = detail_with_relations(
            2,
            "Neighbor",
            "Neighbor",
            Some(26),
            vec![
                related(5, "Already Direct", "Already Direct", "SEQUEL", Some(7)),
                // Also a truly new one.
                related(9, "Genuinely New", "Genuinely New", "SEQUEL", Some(12)),
            ],
        );
        let mut neighbors = std::collections::HashMap::new();
        neighbors.insert(2, neighbor);

        let graft = transitive_relation_graft(&parent, &neighbors);
        assert_eq!(graft.len(), 1, "id=5 must be deduped against direct");
        assert_eq!(graft[0].id, 9);
    }

    #[test]
    fn transitive_graft_filters_adaptation_hops() {
        // Parent's neighbor is a valid PREQUEL. But that neighbor's
        // OWN relations include a manga ADAPTATION. The hop filter
        // (is_pack_candidate_relation) must discard ADAPTATION hops
        // so they're never considered as siblings.
        let parent = detail_with_relations(
            1,
            "Parent",
            "Parent",
            Some(12),
            vec![related(2, "Neighbor", "Neighbor", "PREQUEL", Some(26))],
        );
        let neighbor = detail_with_relations(
            2,
            "Neighbor",
            "Neighbor",
            Some(26),
            vec![
                {
                    let mut r = related(3, "Manga Source", "Manga Source", "ADAPTATION", None);
                    r.media_type = "MANGA".to_string();
                    r
                },
                related(4, "Anime Sequel", "Anime Sequel", "SEQUEL", Some(12)),
            ],
        );
        let mut neighbors = std::collections::HashMap::new();
        neighbors.insert(2, neighbor);

        let graft = transitive_relation_graft(&parent, &neighbors);
        assert_eq!(graft.len(), 1);
        assert_eq!(graft[0].id, 4);
    }

    #[test]
    fn transitive_graft_returns_empty_when_parent_id_is_non_positive() {
        // Provenance gate: don't graft for Jikan-sourced details.
        let parent = detail_with_relations(
            -123,
            "Parent",
            "Parent",
            Some(12),
            vec![related(2, "Neighbor", "Neighbor", "PREQUEL", Some(26))],
        );
        let neighbor = detail_with_relations(
            2,
            "Neighbor",
            "Neighbor",
            Some(26),
            vec![related(3, "Sibling", "Sibling", "SEQUEL", Some(12))],
        );
        let mut neighbors = std::collections::HashMap::new();
        neighbors.insert(2, neighbor);

        let graft = transitive_relation_graft(&parent, &neighbors);
        assert!(graft.is_empty());
    }

    #[test]
    fn transitive_graft_ignores_missing_neighbors_in_map() {
        // If the caller hit the fetch cap and didn't populate every
        // neighbor, graft must silently skip the un-fetched ones.
        let parent = detail_with_relations(
            1,
            "Parent",
            "Parent",
            Some(12),
            vec![
                related(
                    2,
                    "Fetched Neighbor",
                    "Fetched Neighbor",
                    "PREQUEL",
                    Some(26),
                ),
                related(
                    7,
                    "Unfetched Neighbor",
                    "Unfetched Neighbor",
                    "SEQUEL",
                    Some(26),
                ),
            ],
        );
        let neighbor_2 = detail_with_relations(
            2,
            "Fetched Neighbor",
            "Fetched Neighbor",
            Some(26),
            vec![related(5, "New Sibling", "New Sibling", "SEQUEL", Some(12))],
        );
        let mut neighbors = std::collections::HashMap::new();
        neighbors.insert(2, neighbor_2);
        // Note: id=7 is intentionally NOT in the map.

        let graft = transitive_relation_graft(&parent, &neighbors);
        assert_eq!(graft.len(), 1);
        assert_eq!(graft[0].id, 5);
    }

    #[test]
    fn expand_parent_with_transitive_relations_extends_relations_vec() {
        // Integration: the wrapper appends the graft onto the cloned
        // parent's relations vec. Previously-present relations stay.
        let parent = detail_with_relations(
            1,
            "Parent",
            "Parent",
            Some(12),
            vec![related(2, "Neighbor", "Neighbor", "PREQUEL", Some(26))],
        );
        let neighbor = detail_with_relations(
            2,
            "Neighbor",
            "Neighbor",
            Some(26),
            vec![related(3, "Grafted", "Grafted", "SEQUEL", Some(13))],
        );
        let mut neighbors = std::collections::HashMap::new();
        neighbors.insert(2, neighbor);

        let expanded = expand_parent_with_transitive_relations(&parent, &neighbors);
        assert_eq!(expanded.relations.len(), 2);
        let ids: Vec<i64> = expanded.relations.iter().map(|r| r.id).collect();
        assert!(ids.contains(&2), "direct relation must remain");
        assert!(ids.contains(&3), "grafted relation must be appended");
    }

    #[test]
    fn expand_parent_with_empty_graft_still_returns_clone() {
        // When the walk produces no graft (e.g. no walkable direct
        // relations), the wrapper still returns a cloned detail so
        // callers can pass a single owned AnimeDetail into sibling
        // detection regardless.
        let parent = detail_with_relations(1, "Parent", "Parent", Some(12), vec![]);
        let neighbors = std::collections::HashMap::new();
        let expanded = expand_parent_with_transitive_relations(&parent, &neighbors);
        assert_eq!(expanded.id, parent.id);
        assert!(expanded.relations.is_empty());
    }

    #[test]
    fn detect_siblings_finds_grafted_relation_after_transitive_walk() {
        // End-to-end: parent has no direct edge to the sibling whose
        // episodes are in the pack, but a PREQUEL neighbor does. After
        // running the expand step, detect_sibling_entries_in_pack
        // should pick up the grafted sibling. This is the Monogatari
        // missing-edge case modeled structurally.
        let parent_id = 21262;
        let neighbor_id = 20899;
        let grafted_id = 99423;

        let parent = detail_with_relations(
            parent_id,
            "Parent Show",
            "Parent Show",
            Some(12),
            vec![related(
                neighbor_id,
                "Parent Show Franchise",
                "Parent Show Franchise",
                "PREQUEL",
                Some(26),
            )],
        );
        let neighbor = detail_with_relations(
            neighbor_id,
            "Parent Show Franchise",
            "Parent Show Franchise",
            Some(26),
            vec![related(
                grafted_id,
                "Parent Show: Continuation Arc",
                "Parent Show: Continuation Arc",
                "SEQUEL",
                Some(7),
            )],
        );
        let mut neighbors = std::collections::HashMap::new();
        neighbors.insert(neighbor_id, neighbor);

        let expanded = expand_parent_with_transitive_relations(&parent, &neighbors);

        // Subtitle path: filenames mention "Continuation Arc" as the
        // trailing subtitle. Use synthetic tokens — no real group or
        // real release title formatting claimed here.
        let files: Vec<String> = (1..=7)
            .map(|n| {
                format!(
                    "fixture-parent-show continuation arc - {:02} (bd 1080p) [hash].mkv",
                    n
                )
            })
            .collect();

        let siblings = detect_sibling_entries_in_pack(&files, &expanded);
        assert_eq!(
            siblings.len(),
            1,
            "grafted sibling should be detected after transitive walk"
        );
        assert_eq!(siblings[0].anilist_id, grafted_id);
        assert_eq!(siblings[0].file_indices.len(), 7);
    }

    #[test]
    fn detect_siblings_subtitle_path_keeps_offset_zero_for_arc_local_numbering() {
        // Subtitle-matched sibling where filenames use arc-local
        // numbering (E01..Esib_cap within their own arc). The per-
        // sibling offset pass must leave offset=0 because
        // min_ep=1 ≤ parent_cap. Contrived parent with a 2-token,
        // non-ordinal trailing subtitle so the subtitle path fires
        // deterministically.
        let mut parent = detail_with_titles("Parent Show", "Parent Show");
        parent.id = 1;
        parent.episodes = Some(24);
        parent.relations = vec![related(
            2,
            "Parent Show: Alpha Beta",
            "Parent Show: Alpha Beta",
            "SEQUEL",
            Some(12),
        )];
        let files: Vec<String> = (1..=12)
            .map(|n| format!("[Group] Parent Show - Alpha Beta - {:02}.mkv", n))
            .collect();

        let siblings = detect_sibling_entries_in_pack(&files, &parent);
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0].anilist_id, 2);
        assert_eq!(siblings[0].file_indices.len(), 12);
        // min_ep = 1 ≤ parent_cap=24 → offset = 0.
        assert_eq!(siblings[0].episode_offset, 0);
        // And the match came from the subtitle path, not the fallback.
        assert!(
            !siblings[0]
                .matched_subtitle
                .starts_with("episode-range fallback")
        );
    }
}
