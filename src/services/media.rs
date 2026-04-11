use regex_lite::Regex;
use serde::Serialize;
use std::path::Path;

/// Sanitize a string for use as a folder name on disk.
/// Replaces filesystem-unsafe characters and trims leading/trailing dots and whitespace.
pub fn sanitize_folder_name(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim_matches('.')
        .trim()
        .to_string()
}

/// A file found on disk that represents an episode.
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeFile {
    pub filename: String,
    pub episode_number: i32,
    pub season_number: Option<i32>,
    pub quality: String,
    pub size_bytes: u64,
    pub size_display: String,
}

/// Scan a series folder for video files and parse episode info from filenames.
pub fn scan_series_folder(media_root: &str, folder_name: &str) -> Vec<EpisodeFile> {
    if media_root.is_empty() || folder_name.is_empty() {
        return Vec::new();
    }

    let series_path = Path::new(media_root).join(folder_name);
    if !series_path.is_dir() {
        return Vec::new();
    }

    let mut files = Vec::new();
    scan_dir_recursive(&series_path, &series_path, &mut files);

    // Sort by season then episode number.
    files.sort_by(|a, b| {
        a.season_number
            .unwrap_or(0)
            .cmp(&b.season_number.unwrap_or(0))
            .then(a.episode_number.cmp(&b.episode_number))
    });

    files
}

/// List top-level directories in the media root.
pub fn list_media_folders(media_root: &str) -> Vec<String> {
    if media_root.is_empty() {
        return Vec::new();
    }

    let root = Path::new(media_root);
    if !root.is_dir() {
        return Vec::new();
    }

    let mut folders = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    folders.push(name.to_string());
                }
            }
        }
    }
    folders.sort_by_key(|a| a.to_lowercase());
    folders
}

fn scan_dir_recursive(dir: &Path, series_root: &Path, files: &mut Vec<EpisodeFile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, series_root, files);
        } else if is_video_file(&path) {
            if let Some(ep) = parse_episode_file(&path, series_root) {
                files.push(ep);
            }
        }
    }
}

fn is_video_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext.to_lowercase().as_str(),
            "mkv" | "mp4" | "avi" | "wmv" | "flv" | "webm" | "m4v" | "ts"
        ),
        None => false,
    }
}

fn parse_episode_file(path: &Path, series_root: &Path) -> Option<EpisodeFile> {
    let basename = path.file_name()?.to_str()?.to_string();
    let lower = basename.to_lowercase();

    let (season, episode) = parse_episode_number(&lower)?;

    let quality = parse_quality(&lower);

    let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let size_display = format_size(size_bytes);

    // Store relative path from series root so delete handler can reconstruct full path.
    let filename = path
        .strip_prefix(series_root)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or(&basename)
        .to_string();

    Some(EpisodeFile {
        filename,
        episode_number: episode,
        season_number: season,
        quality,
        size_bytes,
        size_display,
    })
}

/// Parse episode (and optionally season) number from a filename.
/// Handles patterns like:
/// - `S01E05`, `S1E5`, `s01e05`
/// - `- 05 (v2)`, `- 05v2`, `[group] Title - 05 [1080p]`
/// - `E05`, `EP05`, `Ep.05`
/// - `Episode 05`
pub fn parse_episode_number(lower: &str) -> Option<(Option<i32>, i32)> {
    // SxxExx pattern — most reliable.
    let re_sxex = Regex::new(r"s(\d{1,2})e(\d{1,4})").unwrap();
    if let Some(caps) = re_sxex.captures(lower) {
        let s: i32 = caps.get(1)?.as_str().parse().ok()?;
        let e: i32 = caps.get(2)?.as_str().parse().ok()?;
        return Some((Some(s), e));
    }

    // " - 05" pattern (common in anime releases from SubsPlease, Erai, etc).
    let re_dash = Regex::new(r" - (\d{1,4})(?:v\d)?(?:\s|\.|\[|\(|$)").unwrap();
    if let Some(caps) = re_dash.captures(lower) {
        let e: i32 = caps.get(1)?.as_str().parse().ok()?;
        return Some((None, e));
    }

    // "E05" or "EP05" or "Ep.05" pattern.
    let re_ep = Regex::new(r"(?:^|[\s._\-])e(?:p\.?)?(\d{1,4})(?:v\d)?(?:\s|\.|\[|\(|$)").unwrap();
    if let Some(caps) = re_ep.captures(lower) {
        let e: i32 = caps.get(1)?.as_str().parse().ok()?;
        return Some((None, e));
    }

    // "Episode 05" pattern.
    let re_episode = Regex::new(r"episode\s*(\d{1,4})").unwrap();
    if let Some(caps) = re_episode.captures(lower) {
        let e: i32 = caps.get(1)?.as_str().parse().ok()?;
        return Some((None, e));
    }

    None
}

/// Extract quality/resolution from filename.
fn parse_quality(lower: &str) -> String {
    // Source type.
    let source = if lower.contains("bluray") || lower.contains("blu-ray") || lower.contains("bdrip") || lower.contains("[bd") || lower.contains("(bd") {
        "Bluray"
    } else if lower.contains("webdl") || lower.contains("web-dl") || lower.contains("web dl") {
        "WEBDL"
    } else if lower.contains("webrip") || lower.contains("web-rip") {
        "WEBRip"
    } else if lower.contains("hdtv") {
        "HDTV"
    } else if lower.contains("dvdrip") || lower.contains("dvd") {
        "DVD"
    } else {
        ""
    };

    // Resolution.
    let res = if lower.contains("2160p") || lower.contains("4k") || lower.contains("uhd") {
        "2160p"
    } else if lower.contains("1080p") || lower.contains("1080") {
        "1080p"
    } else if lower.contains("720p") || lower.contains("720") {
        "720p"
    } else if lower.contains("480p") || lower.contains("480") {
        "480p"
    } else {
        ""
    };

    match (source, res) {
        ("", "") => String::new(),
        ("", r) => r.to_string(),
        (s, "") => s.to_string(),
        (s, r) => format!("{}-{}", s, r),
    }
}

fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{:.1} GiB", gb)
    } else {
        let mb = bytes as f64 / (1024.0 * 1024.0);
        format!("{:.0} MiB", mb)
    }
}
