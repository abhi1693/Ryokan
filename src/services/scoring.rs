use crate::services::nyaa::{SearchOptions, SearchResult};

/// Score a search result based on multiple factors.
pub fn score_result(r: &SearchResult, opts: &SearchOptions) -> i32 {
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
                // Strongly prefer explicit group matches so auto-search does not drift to another group
                // just because it has a few more seeders.
                score += 140 - (idx * 20);
            } else {
                // Slight penalty when the user expressed a group preference but this result is from another group.
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

    // Penalize dual audio.
    if lower.contains("dual audio") || lower.contains("dual.audio") || lower.contains("multi") {
        score -= 5;
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
