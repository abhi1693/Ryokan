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
mod tests;
