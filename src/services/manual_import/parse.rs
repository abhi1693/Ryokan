//! Per-file parsing for the manual-import wizard (#122): which series
//! a file belongs to, and which episode it is.
//!
//! Title extraction reuses `library_link::extract_anime_title` (the
//! anitomy `AnimeTitle` token the grab-time resolver already trusts)
//! and falls back to the parent folder when the filename carries no
//! title at all (`01.mkv`, `S01E05.mkv`, `Episode 07.mkv`), skipping
//! season-style folders (`Season 01`, `Specials`) on the way up so
//! `Anime/Naruto/Season 01/01.mkv` resolves to "Naruto". Episode and
//! season numbers come from `media::parse_episode_number`, the same
//! parser the library scan uses, so the preview's `S01E07` is the one
//! the import would write.

use std::path::Path;
use std::sync::LazyLock;

use anitomy::{Anitomy, ElementCategory};
use regex_lite::Regex;

use crate::services::{library_link, media};

/// Where the series-title hint for a file came from. Shown in the
/// preview so a wrong match is explainable ("it read the folder name").
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum TitleSource {
    Filename,
    ParentFolder,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedFile {
    pub title: Option<String>,
    pub title_source: TitleSource,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    /// Year hint from the filename or, failing that, the folder the
    /// title came from. Feeds the match ranking; never persisted.
    pub year: Option<i32>,
    pub group: Option<String>,
}

/// Folder names that group episodes rather than name a series. The
/// parent-folder fallback climbs past these.
static RE_SEASON_FOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:season|series|s)\s*\d{1,3}|specials?|extras?|ovas?|movies?|bonus|nc(?:op|ed)s?|disc\s*\d+|bd\s*\d+|vol(?:ume)?\.?\s*\d+|subs?|raws?|batch)$",
    )
    .expect("RE_SEASON_FOLDER compiles")
});

/// A four-digit year in the 1900s/2000s with a non-digit on both sides
/// (so `1080p` and `S2024E01`-style runs don't match).
static RE_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^0-9])((?:19|20)\d{2})(?:[^0-9]|$)").expect("RE_YEAR compiles")
});

/// anitomy "titles" that are really episode markers. `Episode 07.mkv`
/// parses with `AnimeTitle = "Episode"`; that must fall through to the
/// folder.
const NOT_A_TITLE: &[&str] = &[
    "episode", "episodes", "ep", "e", "ova", "ona", "oad", "special", "specials", "movie",
    "season", "part", "volume", "vol", "disc", "bonus", "extra", "extras",
];

fn looks_like_title(s: &str) -> bool {
    let t = s.trim();
    if !t.chars().any(|c| c.is_alphabetic()) {
        return false;
    }
    let lower = t.to_lowercase();
    !NOT_A_TITLE.contains(&lower.as_str())
}

pub fn is_season_folder(name: &str) -> bool {
    RE_SEASON_FOLDER.is_match(name.trim().to_lowercase().as_str())
}

/// First plausible year in `s`.
pub fn year_hint(s: &str) -> Option<i32> {
    RE_YEAR
        .captures(s)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Strip bracket / paren groups and collapse whitespace: the raw-folder
/// fallback when anitomy can't find a title in a folder name like
/// `[Judas] Naruto (2002) [BD 1080p]` (anitomy usually can; this is
/// the last resort).
fn clean_folder_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0i32;
    for c in name.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Release-shaped tokens that mean a folder name carries more than a
/// title (`Naruto Shippuden 1080p BD`) and needs anitomy to trim it.
static RE_RELEASE_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[^a-z0-9])(?:\d{3,4}p|bd|bluray|blu-ray|bdrip|bdmv|web|webrip|web-dl|webdl|dvd|dvdrip|hdtv|x264|x265|h264|h265|h\.264|h\.265|hevc|avc|av1|flac|aac|opus|dual|remux|10bit|8bit|hi10p|batch|complete)(?:[^a-z0-9]|$)",
    )
    .expect("RE_RELEASE_TOKEN compiles")
});

/// Title hint from a folder name. Bracket groups (`[Group]`, `(2019)`,
/// `[BD 1080p]`) are stripped first; when nothing release-shaped is
/// left the remainder IS the title. anitomy only runs when release
/// tokens survive the strip, because on a bare folder name it reads a
/// trailing number as an episode (`Mob Psycho 100` becomes `Mob
/// Psycho`), and folders don't carry episode numbers.
fn title_from_folder(name: &str) -> Option<String> {
    let cleaned = clean_folder_name(name);
    if cleaned.is_empty() {
        return None;
    }
    if !RE_RELEASE_TOKEN.is_match(&cleaned) {
        return looks_like_title(&cleaned).then_some(cleaned);
    }
    if let Some(t) = library_link::extract_anime_title(&cleaned)
        && looks_like_title(&t)
    {
        return Some(t);
    }
    looks_like_title(&cleaned).then_some(cleaned)
}

/// Walk `rel_path`'s ancestors nearest-first, returning the first
/// folder that names a series (and that folder's raw name for the
/// year hint).
fn parent_title(rel_path: &Path) -> Option<(String, String)> {
    let mut cur = rel_path.parent();
    while let Some(dir) = cur {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            if !is_season_folder(name)
                && let Some(t) = title_from_folder(name)
            {
                return Some((t, name.to_string()));
            }
        } else {
            break;
        }
        cur = dir.parent();
    }
    None
}

/// Parse one candidate file. `rel_path` is relative to the walk root
/// so the folder fallback never climbs above what the user pointed at.
pub fn parse_file(rel_path: &Path) -> ParsedFile {
    let file_name = rel_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let stem = Path::new(&file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&file_name)
        .to_string();

    // One anitomy pass for the title/year/group trio. The project's
    // tuned episode parser (`media::parse_episode_number`) owns the
    // episode/season decision, not anitomy's EpisodeNumber.
    let clean = if file_name.contains('\0') {
        file_name.replace('\0', "")
    } else {
        file_name.clone()
    };
    let mut ani = Anitomy::new();
    let elements = match ani.parse(&clean) {
        Ok(e) => e,
        Err(e) => e,
    };
    let mut title = elements
        .get(ElementCategory::AnimeTitle)
        .map(|s| s.trim().to_string())
        .filter(|s| looks_like_title(s));
    let mut year = elements
        .get(ElementCategory::AnimeYear)
        .and_then(|y| y.trim().parse::<i32>().ok())
        .or_else(|| year_hint(&stem));
    let group = elements
        .get(ElementCategory::ReleaseGroup)
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty());

    let (season, episode) = match media::parse_episode_number(&file_name.to_lowercase()) {
        Some((s, e)) => (s, Some(e)),
        None => (None, None),
    };

    let mut title_source = if title.is_some() {
        TitleSource::Filename
    } else {
        TitleSource::None
    };
    if title.is_none()
        && let Some((t, folder_raw)) = parent_title(rel_path)
    {
        title = Some(t);
        title_source = TitleSource::ParentFolder;
        if year.is_none() {
            year = year_hint(&folder_raw);
        }
    }

    ParsedFile {
        title,
        title_source,
        season,
        episode,
        year,
        group,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ParsedFile {
        parse_file(Path::new(s))
    }

    #[test]
    fn release_style_filename_parses_title_episode_group_from_name() {
        let f = p("Downloads/[SubsPlease] Frieren - Sousou no Frieren - 05 (1080p) [ABCD1234].mkv");
        assert_eq!(f.title.as_deref(), Some("Frieren - Sousou no Frieren"));
        assert_eq!(f.title_source, TitleSource::Filename);
        assert_eq!(f.episode, Some(5));
        assert_eq!(f.group.as_deref(), Some("SubsPlease"));
    }

    #[test]
    fn bare_number_filename_falls_back_to_parent_folder_past_season_dir() {
        let f = p("Anime/Naruto/Season 01/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Naruto"));
        assert_eq!(f.title_source, TitleSource::ParentFolder);
        assert_eq!(f.episode, Some(1));
    }

    #[test]
    fn sxxexx_filename_uses_folder_title_and_keeps_season() {
        let f = p("Mob Psycho 100/S02E03.mkv");
        assert_eq!(f.title.as_deref(), Some("Mob Psycho 100"));
        assert_eq!(f.title_source, TitleSource::ParentFolder);
        assert_eq!(f.season, Some(2));
        assert_eq!(f.episode, Some(3));
    }

    #[test]
    fn episode_word_is_not_a_title() {
        let f = p("Cowboy Bebop/Episode 07.mkv");
        assert_eq!(f.title.as_deref(), Some("Cowboy Bebop"));
        assert_eq!(f.title_source, TitleSource::ParentFolder);
        assert_eq!(f.episode, Some(7));
    }

    #[test]
    fn year_hint_comes_from_filename_then_folder() {
        let f = p("Hunter x Hunter (2011)/Specials/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Hunter x Hunter"));
        assert_eq!(f.year, Some(2011));

        let f = p("Show/[Group] Show (2019) - 01 [1080p].mkv");
        assert_eq!(f.year, Some(2019));
        assert_eq!(f.episode, Some(1));
    }

    #[test]
    fn resolution_is_not_a_year() {
        assert_eq!(year_hint("Show - 01 [1080p x264]"), None);
        assert_eq!(year_hint("Show 2160p"), None);
        assert_eq!(year_hint("Show (2016) 1080p"), Some(2016));
    }

    #[test]
    fn no_title_anywhere_yields_none() {
        let f = p("Season 01/01.mkv");
        assert_eq!(f.title, None);
        assert_eq!(f.title_source, TitleSource::None);
        assert_eq!(f.episode, Some(1));
    }

    #[test]
    fn season_folder_detection() {
        for s in [
            "Season 01",
            "season 2",
            "S1",
            "Specials",
            "Extras",
            "OVA",
            "Disc 1",
            "Vol. 3",
        ] {
            assert!(is_season_folder(s), "{s} should be a season-style folder");
        }
        for s in ["Naruto", "Season of Love", "86 Eighty Six", "S.A.O"] {
            assert!(
                !is_season_folder(s),
                "{s} should not be a season-style folder"
            );
        }
    }

    #[test]
    fn bracketed_folder_names_clean_up() {
        let f = p("[Judas] Vinland Saga (2019) [BD 1080p]/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Vinland Saga"));
        assert_eq!(f.year, Some(2019));
    }

    #[test]
    fn folder_names_with_release_tokens_but_no_brackets_get_trimmed() {
        let f = p("Naruto Shippuden 1080p BD Dual Audio/01.mkv");
        assert_eq!(f.title.as_deref(), Some("Naruto Shippuden"));
    }

    #[test]
    fn folder_trailing_number_is_part_of_the_title() {
        let f = p("86 Eighty Six/01.mkv");
        assert_eq!(f.title.as_deref(), Some("86 Eighty Six"));
        let f = p("Steins;Gate 0/S01E01.mkv");
        assert_eq!(f.title.as_deref(), Some("Steins;Gate 0"));
    }

    #[test]
    fn ncop_files_have_no_episode_but_keep_title() {
        let f = p("Show/[Group] Show - NCOP1 [1080p].mkv");
        assert_eq!(f.episode, None);
        assert!(f.title.is_some());
    }
}
