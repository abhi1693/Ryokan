use crate::services::nyaa::{SearchOptions, SearchResult};

/// Score a search result based on multiple factors.
/// `prefer_subs` controls whether dual audio/dub releases are penalized (default true).
#[allow(dead_code)]
pub fn score_result(r: &SearchResult, opts: &SearchOptions) -> i32 {
    score_result_with_sub_pref(r, opts, true)
}

pub fn score_result_with_sub_pref(r: &SearchResult, opts: &SearchOptions, prefer_subs: bool) -> i32 {
    let mut score: i32 = 0;

    // Seeders.
    if r.seeders > 100 {
        score += 30;
    } else if r.seeders > 50 {
        score += 25;
    } else if r.seeders > 10 {
        score += 20;
    } else if r.seeders > 0 {
        score += 10;
    } else {
        score -= 10;
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
                score += 140 - (idx * 20);
            } else {
                score -= 15;
            }
        } else {
            score -= 10;
        }
    }

    // Preferred resolution.
    if !opts.preferred_resolution.is_empty() && r.resolution == opts.preferred_resolution {
        score += 20;
    }

    // Batch bonus.
    if r.is_batch {
        score += 15;
    }

    // Trusted bonus.
    if r.is_trusted {
        score += 10;
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
        score += 5;
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
    let is_dub = is_dual || lower.contains("dub") || lower.contains("dubbed") || lower.contains("english dub");
    if prefer_subs {
        // Penalize dub/dual audio releases when user prefers subs.
        if is_dub {
            score -= 15;
        }
    } else {
        // Boost dub/dual audio when user prefers dubs.
        if is_dub {
            score += 15;
        }
    }

    // Downloads popularity.
    if r.downloads > 10000 {
        score += 15;
    } else if r.downloads > 5000 {
        score += 10;
    } else if r.downloads > 1000 {
        score += 5;
    }

    // Small batch bonus (under ~25GB).
    if r.is_batch && r.size_bytes > 0 && r.size_bytes < 25 * 1024 * 1024 * 1024 {
        score += 10;
    }

    score
}
