use std::path::Path;

use crate::models::series::Series;

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
pub fn write_series_nfo(path: &Path, series: &Series) -> std::io::Result<()> {
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
         \x20\x20<status>{status}</status>\n\
         \x20\x20<genre>Animation</genre>\n\
         \x20\x20<uniqueid type=\"anilist\" default=\"true\">{al}</uniqueid>\n",
        title = title,
        orig = orig,
        status = status,
        al = series.anilist_id,
    );

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
pub fn write_episode_nfo(
    path: &Path,
    showtitle: &str,
    season: i32,
    episode: i32,
    ep_title: &str,
    aired: &str,
) -> std::io::Result<()> {
    let display_title = if ep_title.trim().is_empty() {
        format!("Episode {}", episode)
    } else {
        ep_title.to_string()
    };

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <episodedetails>\n\
         \x20\x20<title>{title}</title>\n\
         \x20\x20<showtitle>{show}</showtitle>\n\
         \x20\x20<season>{season}</season>\n\
         \x20\x20<episode>{episode}</episode>\n\
         \x20\x20<aired>{aired}</aired>\n\
         </episodedetails>\n",
        title = xml_escape(&display_title),
        show = xml_escape(showtitle),
        season = season,
        episode = episode,
        aired = aired,
    );

    std::fs::write(path, xml)
}
