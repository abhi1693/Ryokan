//! `walk_video_files` coverage — the directory-walk fallback used
//! when a download client's `get_files` API returns empty for a
//! completed torrent. SAB is the motivating case (its
//! `mode=get_files` only works for queue items; once a job moves
//! to history the file list is gone, leaving Ryokan unable to
//! discover the extracted .mkv).
//!
//! Tests use real `tempfile::TempDir` directories to exercise the
//! actual filesystem walker, since the function is intrinsically
//! filesystem-coupled — mocking would just re-implement the
//! walker without testing it.

use std::fs;
use std::io::Write;

use crate::services::post_processing::walk_video_files;

/// SAB's canonical extraction shape: `<storage>/<filename>` with
/// the storage directory named after the NZB. Most common case for
/// single-episode grabs from NZBGeek-style indexers. The walker
/// must find the .mkv one level down without configuration.
#[test]
fn walks_one_level_deep_for_sab_shape() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = tmp.path();
    let mut f = fs::File::create(storage.join("[Erai-raws].One.Piece-1158.mkv")).unwrap();
    f.write_all(b"fake mkv contents").unwrap();
    drop(f);

    let files = walk_video_files(storage);
    assert_eq!(files.len(), 1, "expected one .mkv at the storage root");
    assert_eq!(files[0].name, "[Erai-raws].One.Piece-1158.mkv");
    assert_eq!(files[0].progress, 1.0);
    assert!(files[0].wanted);
    assert_eq!(files[0].size, b"fake mkv contents".len() as i64);
}

/// Non-video files in the storage directory must be filtered out.
/// SAB jobs often leave behind `.nfo`, `.srt`, sample folders, and
/// par2 / sfv leftovers post-extraction. The import path doesn't
/// want any of those mistaken for the main video.
#[test]
fn filters_non_video_extensions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = tmp.path();
    fs::write(storage.join("show.mkv"), b"video").unwrap();
    fs::write(storage.join("show.nfo"), b"metadata").unwrap();
    fs::write(storage.join("show.srt"), b"subtitles").unwrap();
    fs::write(storage.join("readme.txt"), b"notes").unwrap();
    fs::write(storage.join("show.par2"), b"parity").unwrap();

    let files = walk_video_files(storage);
    assert_eq!(files.len(), 1, "only the .mkv must survive the filter");
    assert_eq!(files[0].name, "show.mkv");
}

/// Multi-file SAB extractions (rare for single-episode anime, but
/// more common for batch packs) must surface every video. Names
/// come back as paths relative to the walk root so the caller can
/// `Path::new(&source_base).join(&file.name)` without prefix
/// surprises.
#[test]
fn walks_multi_file_storage() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = tmp.path();
    fs::write(storage.join("E01.mkv"), b"a").unwrap();
    fs::write(storage.join("E02.mkv"), b"b").unwrap();
    fs::write(storage.join("E03.mp4"), b"c").unwrap();

    let mut files = walk_video_files(storage);
    files.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].name, "E01.mkv");
    assert_eq!(files[1].name, "E02.mkv");
    assert_eq!(files[2].name, "E03.mp4");
}

/// BT-shape multi-file torrents: a torrent name dir with the
/// videos inside. `walk_video_files` recurses one level deeper,
/// returning paths relative to the walk root. Caller's
/// `source_base + name` resolves to the absolute file location.
/// The relative-name shape lets the import loop rename files
/// without losing the per-torrent directory context.
#[test]
fn returns_paths_relative_to_walk_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = tmp.path();
    let inner = storage.join("[smol] Monogatari Series");
    fs::create_dir(&inner).unwrap();
    fs::write(inner.join("01.mkv"), b"a").unwrap();
    fs::write(inner.join("02.mkv"), b"b").unwrap();

    let mut files = walk_video_files(storage);
    files.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(files.len(), 2);
    // On Unix the path separator is `/`. On Windows it would be
    // `\`. The test is Unix-only via the `cfg!(unix)` guard below
    // since CI runs on Linux. The relative-path semantic is what
    // matters; the separator is platform-dependent.
    if cfg!(unix) {
        assert_eq!(files[0].name, "[smol] Monogatari Series/01.mkv");
        assert_eq!(files[1].name, "[smol] Monogatari Series/02.mkv");
    }
}

/// Empty storage directory returns empty Vec without erroring.
/// Edge case: SAB extraction in progress, or a job whose post-
/// processing failed and left an empty dir behind.
#[test]
fn empty_directory_returns_empty_vec() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let files = walk_video_files(tmp.path());
    assert!(files.is_empty());
}

/// Non-existent path must NOT panic — `read_dir` returns Err which
/// the function silently skips. Edge case: the path translation
/// resolved to a directory that doesn't exist on Ryokan's host
/// (misconfigured download_path on the SAB row).
#[test]
fn nonexistent_path_returns_empty_vec_without_panic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bogus = tmp.path().join("does-not-exist");
    let files = walk_video_files(&bogus);
    assert!(files.is_empty());
}

/// Recursion guard — a deeply-nested archive shouldn't hang the
/// walker. The cap is 4 levels; videos beyond that are silently
/// skipped. Tested via a 6-deep nesting where only the levels
/// within the cap should surface.
#[test]
fn caps_recursion_depth() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = tmp.path();
    fs::write(storage.join("d0.mkv"), b"x").unwrap();
    let mut path = storage.to_path_buf();
    for i in 1..=6 {
        path = path.join(format!("level{i}"));
        fs::create_dir(&path).unwrap();
        fs::write(path.join(format!("d{i}.mkv")), b"x").unwrap();
    }
    let mut files = walk_video_files(storage);
    files.sort_by(|a, b| a.name.cmp(&b.name));
    // depth 0 (storage root): d0.mkv
    // depth 1: level1/d1.mkv
    // depth 2: level1/level2/d2.mkv
    // depth 3: level1/level2/level3/d3.mkv
    // depth 4: level1/level2/level3/level4/d4.mkv
    // depth 5+: not walked
    assert_eq!(
        files.len(),
        5,
        "expected videos at depths 0-4 inclusive; got {} entries: {:?}",
        files.len(),
        files.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
}

/// Extension matching is case-insensitive — `show.MKV` and
/// `show.Mkv` should both surface the same as `show.mkv`. SAB
/// occasionally yields capitalized extensions on releases from
/// Windows-side rippers; without case folding those would be
/// silently filtered out by `is_video_file` and the import
/// would never trigger.
#[test]
fn case_insensitive_extension_match() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = tmp.path();
    fs::write(storage.join("upper.MKV"), b"a").unwrap();
    fs::write(storage.join("mixed.Mp4"), b"b").unwrap();

    let mut files = walk_video_files(storage);
    files.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(files.len(), 2);
}
