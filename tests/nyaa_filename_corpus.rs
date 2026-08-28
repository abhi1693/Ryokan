//! Characterization corpus for `parse_episode_number` over real-world Nyaa
//! filenames. Fixtures live in `tests/fixtures/nyaa-filenames/` (one JSON per
//! torrent; see the README there for provenance and category buckets).
//!
//! Expectations are snapshots of current behavior, including known
//! mis-parses of extras. A parser change that shifts any real-world outcome
//! fails here so the shift is reviewed deliberately rather than discovered
//! in someone's library.

use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    title: String,
    files: Vec<FileCase>,
}

#[derive(Deserialize)]
struct FileCase {
    name: String,
    season: Option<i32>,
    episode: Option<i32>,
}

#[test]
fn corpus_parse_results_are_stable() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nyaa-filenames");
    let mut fixture_count = 0_usize;
    let mut case_count = 0_usize;
    let mut failures: Vec<String> = Vec::new();

    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("fixture dir exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    for path in paths {
        let raw = std::fs::read_to_string(&path).expect("read fixture");
        let fixture: Fixture = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("bad fixture {}: {}", path.display(), e));
        fixture_count += 1;

        for case in &fixture.files {
            case_count += 1;
            let expected = case.episode.map(|e| (case.season, e));
            let got = ryokan::services::media::parse_episode_number(&case.name.to_lowercase());
            if got != expected {
                failures.push(format!(
                    "{} :: '{}' parsed {:?}, fixture expects {:?}",
                    fixture.title, case.name, got, expected
                ));
            }
        }
    }

    assert!(
        fixture_count >= 15,
        "corpus unexpectedly small: {fixture_count}"
    );
    assert!(
        case_count >= 300,
        "corpus unexpectedly small: {case_count} cases"
    );
    assert!(
        failures.is_empty(),
        "{} corpus deviations:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
