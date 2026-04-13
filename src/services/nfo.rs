use std::path::Path;

use crate::models::series::Series;
use crate::services::anilist::AnimeDetail;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Strip basic HTML tags out of an AniList description for use inside a
/// `<plot>` tag. AniList descriptions arrive with `<br>`, `<i>`, etc.; the
/// rich-description sanitizer leaves them in but the NFO consumer (Jellyfin)
/// renders the `<plot>` body verbatim, so the tags would show up in the UI.
///
/// Removed tags are replaced with a single space so structural tags like
/// `<br>` between sentences keep acting as word separators. Adjacent runs of
/// whitespace are then collapsed.
///
/// If the input ends mid-tag (an unmatched trailing `<`), the buffered chars
/// that came after that `<` are flushed as literal text — they weren't really
/// a tag, the `<` was just a stray bracket, and dropping the rest would
/// silently eat content from malformed descriptions. The leading `<` itself
/// is not re-emitted, matching the "strip markup, keep text" spirit of the
/// function.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // Buffer for characters that appeared after a `<` — discarded when a
    // matching `>` closes the tag, or flushed as literal content when the
    // input ends before `>` arrives (or a second `<` restarts tag-mode).
    let mut tag_buf = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => {
                // Nested/unmatched `<` — flush the prior false-start as
                // literal before entering the new tag.
                if in_tag && !tag_buf.is_empty() {
                    out.push_str(&tag_buf);
                    tag_buf.clear();
                }
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => {
                in_tag = false;
                tag_buf.clear();
            }
            _ if !in_tag => out.push(ch),
            _ => tag_buf.push(ch),
        }
    }
    // Input ended with an unmatched `<foo` — treat the remainder as literal.
    if in_tag && !tag_buf.is_empty() {
        out.push_str(&tag_buf);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Best display title for a series: English → Romaji → title.
pub fn best_title(series: &Series) -> String {
    if !series.title_english.is_empty() {
        series.title_english.clone()
    } else if !series.title_romaji.is_empty() {
        series.title_romaji.clone()
    } else {
        series.title.clone()
    }
}

/// Write a `tvshow.nfo` file to `path` using data already stored in the DB.
/// Jellyfin reads this instead of querying any external metadata provider.
///
/// When `detail` is provided (the cached `AnimeDetail` from the metadata
/// cache), the NFO is enriched with plot, year, premiered, rating, genres,
/// and runtime. Without it the output is the minimal series-row-only form
/// used as a fallback when the metadata cache is empty.
pub fn write_series_nfo(
    path: &Path,
    series: &Series,
    detail: Option<&AnimeDetail>,
) -> std::io::Result<()> {
    let title = xml_escape(&best_title(series));
    let orig = xml_escape(&series.title_native);
    let status = match series.status.as_str() {
        "FINISHED" | "FINISHED_AIRING" => "Ended",
        "RELEASING" | "CURRENTLY_AIRING" => "Continuing",
        _ => "Unknown",
    };

    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <tvshow>\n\
         \x20\x20<title>{title}</title>\n\
         \x20\x20<originaltitle>{orig}</originaltitle>\n\
         \x20\x20<status>{status}</status>\n",
        title = title,
        orig = orig,
        status = status,
    );

    // Plot: prefer AniList description (HTML-stripped). Falls through silently
    // when the cache is unavailable.
    if let Some(d) = detail {
        let plot = strip_html_tags(&d.description);
        if !plot.trim().is_empty() {
            xml.push_str(&format!("  <plot>{}</plot>\n", xml_escape(plot.trim())));
        }

        // <year> and <premiered>: Jellyfin uses these to sort and to display
        // the year on cards. season_year is the only year field we have, so
        // we synthesize a January 1 premiered date — Jellyfin tolerates the
        // imprecision and uses just the year for display anyway.
        if let Some(year) = d.season_year {
            xml.push_str(&format!("  <year>{}</year>\n", year));
            xml.push_str(&format!("  <premiered>{}-01-01</premiered>\n", year));
        }

        // <rating>: AniList averageScore is 0-100. Convert to /10 so it
        // matches the convention Jellyfin expects from TVDB-sourced ratings.
        if let Some(score) = d.average_score {
            xml.push_str(&format!(
                "  <rating>{:.1}</rating>\n",
                (score as f32) / 10.0
            ));
        }

        // <runtime>: detail.duration is per-episode minutes from AniList.
        if let Some(duration) = d.duration {
            if duration > 0 {
                xml.push_str(&format!("  <runtime>{}</runtime>\n", duration));
            }
        }

        // Real genre tags. Always include "Animation" as a fallback so the
        // category filter still groups it correctly even when AniList genres
        // are sparse.
        let mut emitted_animation = false;
        for genre in &d.genres {
            let trimmed = genre.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.eq_ignore_ascii_case("animation") {
                emitted_animation = true;
            }
            xml.push_str(&format!("  <genre>{}</genre>\n", xml_escape(trimmed)));
        }
        if !emitted_animation {
            xml.push_str("  <genre>Animation</genre>\n");
        }
    } else {
        // Minimal fallback when the cache is missing.
        xml.push_str("  <genre>Animation</genre>\n");
    }

    xml.push_str(&format!(
        "  <uniqueid type=\"anilist\" default=\"true\">{}</uniqueid>\n",
        series.anilist_id
    ));

    if let Some(mal_id) = series.mal_id {
        xml.push_str(&format!(
            "  <uniqueid type=\"myanimelist\">{}</uniqueid>\n",
            mal_id
        ));
    }

    xml.push_str("</tvshow>\n");

    std::fs::write(path, xml)
}

/// Write an episode `.nfo` alongside the renamed video file.
/// `path` should have a `.nfo` extension.
/// Falls back gracefully when title or air date are unavailable.
///
/// `runtime_minutes` is the per-episode runtime from the cached series
/// detail, or `None` when unknown. Jellyfin shows "Unknown" duration on
/// episode cards until it has scanned the file once, so emitting the
/// AniList runtime up-front is a meaningful UX improvement.
pub fn write_episode_nfo(
    path: &Path,
    showtitle: &str,
    season: i32,
    episode: i32,
    ep_title: &str,
    aired: &str,
    runtime_minutes: Option<i32>,
) -> std::io::Result<()> {
    let display_title = if ep_title.trim().is_empty() {
        format!("Episode {}", episode)
    } else {
        ep_title.to_string()
    };

    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <episodedetails>\n\
         \x20\x20<title>{title}</title>\n\
         \x20\x20<showtitle>{show}</showtitle>\n\
         \x20\x20<season>{season}</season>\n\
         \x20\x20<episode>{episode}</episode>\n\
         \x20\x20<aired>{aired}</aired>\n",
        title = xml_escape(&display_title),
        show = xml_escape(showtitle),
        season = season,
        episode = episode,
        aired = aired,
    );

    if let Some(runtime) = runtime_minutes {
        if runtime > 0 {
            xml.push_str(&format!("  <runtime>{}</runtime>\n", runtime));
        }
    }

    xml.push_str("</episodedetails>\n");

    std::fs::write(path, xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_tags_removes_anilist_markup() {
        let input = "First sentence.<br><br>Second sentence with <i>emphasis</i> and a <a href=\"x\">link</a>";
        let out = strip_html_tags(input);
        // Tags gone, structural tags act as word separators, runs of
        // whitespace collapsed. (A trailing tag before punctuation can
        // leave a stray space — see strip_html_tags doc comment — so this
        // test deliberately ends without trailing punctuation.)
        assert_eq!(out, "First sentence. Second sentence with emphasis and a link");
    }

    #[test]
    fn strip_html_tags_handles_unbalanced_input() {
        // Malformed AniList descriptions shouldn't crash or eat characters
        // outside the broken tag. When the input ends mid-tag, the buffered
        // chars are flushed as literal text (without the leading `<`).
        assert_eq!(strip_html_tags("hello <not closed"), "hello not closed");
        assert_eq!(strip_html_tags("plain text"), "plain text");
        // Two consecutive unclosed `<` — the first one's contents are
        // flushed when the second `<` arrives.
        assert_eq!(strip_html_tags("a <bc<d"), "a bc d");
    }

    fn detail_with_everything() -> AnimeDetail {
        AnimeDetail {
            id: 12345,
            id_mal: Some(67890),
            title_romaji: "Romaji Title".to_string(),
            title_english: "English Title".to_string(),
            title_native: "原題".to_string(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes: Some(12),
            duration: Some(24),
            season: "WINTER".to_string(),
            season_year: Some(2024),
            end_year: Some(2024),
            description: "A <i>brilliant</i> story.<br>About things.".to_string(),
            genres: vec!["Action".to_string(), "Drama".to_string()],
            average_score: Some(85),
            average_score_display: Some("85%".to_string()),
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn series_stub() -> Series {
        Series {
            id: 1,
            anilist_id: 12345,
            mal_id: Some(67890),
            title: "English Title".to_string(),
            title_romaji: "Romaji Title".to_string(),
            title_english: "English Title".to_string(),
            title_native: "原題".to_string(),
            cover_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            episodes: Some(12),
            season_year: Some(2024),
            end_year: Some(2024),
            folder_name: "english-title".to_string(),
            monitor_mode: "future".to_string(),
            allow_upgrades: true,
        }
    }

    fn unique_temp_path(suffix: &str) -> std::path::PathBuf {
        let nonce = format!(
            "ryokan_nfo_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            suffix,
        );
        let dir = std::env::temp_dir().join(nonce);
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir.join(suffix)
    }

    fn render_series_nfo(detail: Option<&AnimeDetail>) -> String {
        let path = unique_temp_path("tvshow.nfo");
        write_series_nfo(&path, &series_stub(), detail).expect("write nfo");
        let xml = std::fs::read_to_string(&path).expect("read nfo");
        std::fs::remove_file(&path).ok();
        if let Some(parent) = path.parent() {
            std::fs::remove_dir(parent).ok();
        }
        xml
    }

    #[test]
    fn series_nfo_with_detail_emits_plot_year_rating_genres() {
        let detail = detail_with_everything();
        let xml = render_series_nfo(Some(&detail));

        // Plot is HTML-stripped.
        assert!(xml.contains("<plot>A brilliant story. About things.</plot>"));
        // Year + premiered both emitted from season_year.
        assert!(xml.contains("<year>2024</year>"));
        assert!(xml.contains("<premiered>2024-01-01</premiered>"));
        // 85/100 → 8.5/10.
        assert!(xml.contains("<rating>8.5</rating>"));
        // Per-episode runtime in minutes.
        assert!(xml.contains("<runtime>24</runtime>"));
        // Real genres + the always-on Animation tag.
        assert!(xml.contains("<genre>Action</genre>"));
        assert!(xml.contains("<genre>Drama</genre>"));
        assert!(xml.contains("<genre>Animation</genre>"));
        // Status maps to Jellyfin's vocabulary.
        assert!(xml.contains("<status>Ended</status>"));
        // Identifiers preserved.
        assert!(xml.contains("<uniqueid type=\"anilist\" default=\"true\">12345</uniqueid>"));
        assert!(xml.contains("<uniqueid type=\"myanimelist\">67890</uniqueid>"));
    }

    #[test]
    fn series_nfo_without_detail_falls_back_to_minimal() {
        let xml = render_series_nfo(None);
        // No enrichment fields — just title/originaltitle/status/genre/ids.
        assert!(!xml.contains("<plot>"));
        assert!(!xml.contains("<year>"));
        assert!(!xml.contains("<rating>"));
        assert!(!xml.contains("<runtime>"));
        // Animation fallback still emitted so the category filter behaves.
        assert!(xml.contains("<genre>Animation</genre>"));
        assert!(xml.contains("<title>English Title</title>"));
    }

    #[test]
    fn series_nfo_does_not_double_emit_animation_when_anilist_lists_it() {
        let mut detail = detail_with_everything();
        detail.genres = vec!["Animation".to_string(), "Adventure".to_string()];
        let xml = render_series_nfo(Some(&detail));
        // Animation should appear exactly once (from AniList) — the fallback
        // must not double-emit it.
        assert_eq!(xml.matches("<genre>Animation</genre>").count(), 1);
        assert!(xml.contains("<genre>Adventure</genre>"));
    }

    #[test]
    fn episode_nfo_emits_runtime_when_provided() {
        let path = unique_temp_path("ep_with_runtime.nfo");
        write_episode_nfo(&path, "Show", 1, 5, "The Title", "2024-03-01", Some(24))
            .expect("write nfo");
        let xml = std::fs::read_to_string(&path).expect("read nfo");
        std::fs::remove_file(&path).ok();
        if let Some(parent) = path.parent() {
            std::fs::remove_dir(parent).ok();
        }
        assert!(xml.contains("<runtime>24</runtime>"));
        assert!(xml.contains("<title>The Title</title>"));
        assert!(xml.contains("<aired>2024-03-01</aired>"));
    }

    #[test]
    fn episode_nfo_omits_runtime_when_unknown() {
        let path = unique_temp_path("ep_no_runtime.nfo");
        write_episode_nfo(&path, "Show", 1, 5, "", "", None).expect("write nfo");
        let xml = std::fs::read_to_string(&path).expect("read nfo");
        std::fs::remove_file(&path).ok();
        if let Some(parent) = path.parent() {
            std::fs::remove_dir(parent).ok();
        }
        assert!(!xml.contains("<runtime>"));
        // Empty title falls back to "Episode N".
        assert!(xml.contains("<title>Episode 5</title>"));
    }
}
