use serde::{Deserialize, Serialize};

/// Quality tiers ordered from lowest to highest.
/// The numeric rank is used for comparison — higher is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum QualityTier {
    Unknown = 0,
    Web480 = 10,
    Dvd = 15,
    Bd480 = 20,
    Web720 = 30,
    Bd720 = 40,
    Remux720 = 45,
    Web1080 = 50,
    Bd1080 = 60,
    Remux1080 = 65,
}

impl QualityTier {
    /// Numeric rank for comparison and scoring.
    pub fn rank(self) -> i32 {
        self as i32
    }

    /// Display label for UI.
    pub fn label(self) -> &'static str {
        match self {
            QualityTier::Unknown => "Unknown",
            QualityTier::Web480 => "WEB 480p",
            QualityTier::Dvd => "DVD",
            QualityTier::Bd480 => "BD 480p",
            QualityTier::Web720 => "WEB 720p",
            QualityTier::Bd720 => "BD 720p",
            QualityTier::Remux720 => "BD Remux 720p",
            QualityTier::Web1080 => "WEB 1080p",
            QualityTier::Bd1080 => "BD 1080p",
            QualityTier::Remux1080 => "BD Remux 1080p",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "web_480" => QualityTier::Web480,
            "dvd" => QualityTier::Dvd,
            "bd_480" => QualityTier::Bd480,
            "web_720" => QualityTier::Web720,
            "bd_720" => QualityTier::Bd720,
            "remux_720" => QualityTier::Remux720,
            "web_1080" => QualityTier::Web1080,
            "bd_1080" => QualityTier::Bd1080,
            "remux_1080" => QualityTier::Remux1080,
            _ => QualityTier::Unknown,
        }
    }

    /// Is this tier a Bluray source (BD or Remux)?
    pub fn is_bluray(self) -> bool {
        matches!(
            self,
            QualityTier::Bd480
                | QualityTier::Bd720
                | QualityTier::Bd1080
                | QualityTier::Remux720
                | QualityTier::Remux1080
        )
    }
}

/// What to do for finished series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishedSeriesMode {
    /// Same behavior as airing — grab best available per profile.
    SameAsAiring,
    /// Prefer BD: search for BD first, fall back to WEB if none found.
    PreferBd,
    /// Only grab BD or above — skip WEB entirely for finished series.
    BdOnly,
}

impl FinishedSeriesMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "prefer_bd" => FinishedSeriesMode::PreferBd,
            "bd_only" => FinishedSeriesMode::BdOnly,
            _ => FinishedSeriesMode::SameAsAiring,
        }
    }

}

/// Detect the quality tier of a release from its title and parsed resolution.
pub fn detect_tier(title: &str, resolution: &str) -> QualityTier {
    let lower = title.to_lowercase();

    let is_remux = lower.contains("remux");
    let is_bluray = is_remux
        || lower.contains("bluray")
        || lower.contains("blu-ray")
        || lower.contains("bdrip")
        || lower.contains("bdremux")
        || lower.contains("[bd")
        || lower.contains("(bd")
        || lower.contains(" bd ");
    let is_dvd = lower.contains("dvdrip")
        || lower.contains("dvd")
        || lower.contains("[dvd")
        || lower.contains("(dvd");

    match resolution {
        "1080" => {
            if is_remux {
                QualityTier::Remux1080
            } else if is_bluray {
                QualityTier::Bd1080
            } else {
                QualityTier::Web1080
            }
        }
        "720" => {
            if is_remux {
                QualityTier::Remux720
            } else if is_bluray {
                QualityTier::Bd720
            } else {
                QualityTier::Web720
            }
        }
        "480" | "576" => {
            if is_bluray {
                QualityTier::Bd480
            } else if is_dvd {
                QualityTier::Dvd
            } else {
                QualityTier::Web480
            }
        }
        "2160" => {
            // 4K is always above our tier list — treat as Remux 1080 equivalent.
            QualityTier::Remux1080
        }
        _ => {
            // No resolution detected — guess from source tags.
            if is_remux {
                QualityTier::Remux1080
            } else if is_bluray {
                QualityTier::Bd1080
            } else if is_dvd {
                QualityTier::Dvd
            } else {
                QualityTier::Unknown
            }
        }
    }
}

/// Score bonus/penalty based on how a release's quality tier compares to the
/// preferred tier and cutoff. Returns a score modifier.
pub fn tier_score(detected: QualityTier, preferred: QualityTier, cutoff: QualityTier) -> i32 {
    let det_rank = detected.rank();
    let pref_rank = preferred.rank();
    let cut_rank = cutoff.rank();

    if det_rank == 0 {
        // Unknown tier — small penalty, don't rule it out.
        return -5;
    }

    let mut score = 0;

    // Bonus for matching or exceeding preferred tier.
    if det_rank >= pref_rank {
        score += 30;
        // Extra bonus for exact match.
        if det_rank == pref_rank {
            score += 15;
        }
    } else {
        // Below preferred — penalty proportional to distance.
        let gap = (pref_rank - det_rank) / 10;
        score -= 10 + gap * 8;
    }

    // Bonus for being at or above cutoff.
    if det_rank >= cut_rank {
        score += 10;
    }

    score
}

/// Check if a release's quality tier is acceptable given the finished-series
/// mode. Returns false if the release should be filtered out.
pub fn passes_finished_filter(
    detected: QualityTier,
    mode: FinishedSeriesMode,
    is_finished: bool,
) -> bool {
    if !is_finished {
        return true;
    }
    match mode {
        FinishedSeriesMode::SameAsAiring => true,
        FinishedSeriesMode::PreferBd => true, // Prefer but don't filter.
        FinishedSeriesMode::BdOnly => detected.is_bluray() || detected == QualityTier::Unknown,
    }
}

/// Detect quality tier from an on-disk quality string (as returned by media::parse_quality).
/// Examples: "Bluray-1080p", "1080p", "WEBRip-720p", "WEBDL-1080p", ""
pub fn tier_from_disk_quality(quality: &str) -> QualityTier {
    let lower = quality.to_lowercase();
    let is_bluray = lower.contains("bluray") || lower.contains("bdrip") || lower.contains("bdremux");
    let is_remux = lower.contains("remux");
    let is_dvd = lower.contains("dvd");

    let resolution = if lower.contains("2160") {
        "2160"
    } else if lower.contains("1080") {
        "1080"
    } else if lower.contains("720") {
        "720"
    } else if lower.contains("480") {
        "480"
    } else {
        ""
    };

    detect_tier(
        &if is_remux { "remux".to_string() }
         else if is_bluray { "bluray".to_string() }
         else if is_dvd { "dvd".to_string() }
         else { String::new() },
        resolution,
    )
}

/// Shared preferred-group scoring used by both RSS and auto-search.
/// `preferred_groups` should be ordered by priority (first = most preferred).
/// Returns a score bonus/penalty.
pub fn preferred_group_bonus(group: &str, preferred_groups: &[String]) -> i32 {
    if preferred_groups.is_empty() {
        return 0;
    }
    if group.trim().is_empty() {
        return -15;
    }
    for (idx, preferred) in preferred_groups.iter().enumerate() {
        if preferred.eq_ignore_ascii_case(group.trim()) {
            return 180 - (idx as i32 * 30);
        }
    }
    -40
}

/// Parse a comma-separated group list into a vec of trimmed, non-empty strings.
pub fn parse_group_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Build Nyaa search queries to probe for BD releases of a series.
pub fn bd_probe_queries(aliases: &[String]) -> Vec<String> {
    let mut queries = Vec::new();
    for alias in aliases {
        queries.push(format!("{} bluray", alias));
        queries.push(format!("{} BD", alias));
        queries.push(format!("{} BDRip", alias));
        queries.push(format!("{} remux", alias));
    }
    queries
}
