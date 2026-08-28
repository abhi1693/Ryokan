//! End-to-end coverage for the manual-import matcher's query ladder
//! (#122): when AniList returns nothing for the `title season N`
//! query, the bare title is tried next. Stands up a wiremock AniList
//! and points Ryokan at it via `RYOKAN_ANILIST_API_BASE`, the same
//! fixture shape as `tests/library_link_e2e.rs`.

use std::path::PathBuf;
use std::sync::LazyLock;

use ryokan::services::anilist;
use ryokan::services::manual_import::parse::TitleSource;
use ryokan::services::manual_import::{self, CandidateFile, SeriesGroup};
use serde_json::json;
use tokio::sync::Mutex;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn search_response(entries: &[(i64, &str, &str)]) -> serde_json::Value {
    let media: Vec<serde_json::Value> = entries
        .iter()
        .map(|(id, romaji, english)| {
            json!({
                "id": id,
                "idMal": null,
                "title": { "romaji": romaji, "english": english, "native": "" },
                "coverImage": { "large": "https://example/cover.jpg" },
                "format": "TV",
                "status": "FINISHED",
                "episodes": 12,
                "seasonYear": 2024,
                "averageScore": 80,
            })
        })
        .collect();
    json!({ "data": { "Page": { "media": media } } })
}

fn file(name: &str, ep: i32) -> CandidateFile {
    CandidateFile {
        path: PathBuf::from(name),
        rel_path: name.to_string(),
        file_name: name.to_string(),
        size_bytes: 1,
        parsed_title: Some("Enen no Shouboutai".into()),
        title_source: TitleSource::Filename,
        season: Some(3),
        episode: Some(ep),
        year: None,
        group: None,
        quality_label: "WEB-1080p".into(),
        selected: true,
    }
}

fn group() -> SeriesGroup {
    SeriesGroup {
        key: "enen no shouboutai|s3".into(),
        parsed_title: "Enen no Shouboutai".into(),
        season: Some(3),
        year: None,
        query: "Enen no Shouboutai season 3".into(),
        files: vec![file(
            "[SubsPlease] Enen no Shouboutai S3 - 18 (1080p).mkv",
            18,
        )],
        candidates: Vec::new(),
        pick: None,
        low_confidence: false,
        search_error: None,
        skipped: false,
        existing: None,
    }
}

#[tokio::test]
async fn empty_season_query_falls_back_to_the_bare_title_and_ranks_the_sequel() {
    let _gate = ENV_LOCK.lock().await;
    anilist::reset_state_for_tests();
    let mock = MockServer::start().await;
    unsafe {
        std::env::set_var("RYOKAN_ANILIST_API_BASE", mock.uri());
    }
    // The season-suffixed query comes back empty; the bare title
    // returns the franchise, sequel included.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("season 3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[])))
        .with_priority(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_string_contains("SEARCH_MATCH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
            (1, "Enen no Shouboutai", "Fire Force"),
            (2, "Enen no Shouboutai: Ni no Shou", "Fire Force Season 2"),
            (3, "Enen no Shouboutai: San no Shou", "Fire Force Season 3"),
        ])))
        .with_priority(5)
        .mount(&mock)
        .await;

    let mut g = group();
    manual_import::search_and_rank(&mut g, true).await;
    assert!(g.search_error.is_none(), "{:?}", g.search_error);
    assert_eq!(
        g.query, "Enen no Shouboutai",
        "the query that matched is kept"
    );
    assert_eq!(
        g.picked().map(|e| e.id),
        Some(3),
        "season marker picks the sequel"
    );
    assert!(!g.low_confidence);

    // A typed re-search is searched as typed: no ladder.
    let mut g = group();
    g.query = "Enen no Shouboutai season 3".into();
    manual_import::search_and_rank(&mut g, false).await;
    assert!(g.candidates.is_empty());
    assert!(g.pick.is_none());

    unsafe {
        std::env::remove_var("RYOKAN_ANILIST_API_BASE");
    }
    anilist::reset_state_for_tests();
}
