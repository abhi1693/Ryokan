//! `is_video_file` + `sanitize_filename` coverage. Both are pure
//! helpers that filter/rewrite byte strings — tests here pin the
//! extensions list and the sanitize behavior so a future tweak
//! (adding a new extension, changing the unsafe-char set) lands
//! with a paired test update rather than a silent regression.

use rstest::rstest;

use crate::services::post_processing::{is_video_file, sanitize_filename};

// ─── is_video_file extension table ────────────────────────────────

#[rstest]
#[case("show.mkv", true)]
#[case("show.mp4", true)]
#[case("show.avi", true)]
#[case("show.wmv", true)]
#[case("show.webm", true)]
#[case("show.m4v", true)]
#[case("show.ts", true)]
fn accepts_known_video_extensions(#[case] name: &str, #[case] expected: bool) {
    assert_eq!(is_video_file(name), expected, "name: {name}");
}

#[rstest]
#[case("readme.txt")]
#[case("show.nfo")]
#[case("sample.srt")]
#[case("poster.jpg")]
#[case("cover.png")]
#[case("dump.sqlite")]
fn rejects_non_video_extensions(#[case] name: &str) {
    assert!(!is_video_file(name), "name: {name}");
}

#[test]
fn is_case_insensitive_on_extension() {
    assert!(is_video_file("Show.MKV"));
    assert!(is_video_file("show.Mp4"));
    assert!(is_video_file("show.M4V"));
}

#[test]
fn bare_name_with_no_extension_is_not_a_video() {
    // A filename without an extension can't be classified by
    // extension alone — treat as non-video.
    assert!(!is_video_file("README"));
    assert!(!is_video_file("noext"));
}

#[test]
fn path_components_do_not_leak_into_extension_match() {
    // Full path components (e.g. "show.mkv/meta") shouldn't match
    // as a .mkv file — only the trailing component's extension
    // matters. Rust's Path::extension handles this correctly; we
    // pin the behavior.
    assert!(!is_video_file("show.mkv/metadata.txt"));
    assert!(is_video_file("folder.other/show.mkv"));
}

// ─── sanitize_filename ─────────────────────────────────────────────

#[test]
fn sanitize_filename_preserves_plain_ascii() {
    assert_eq!(sanitize_filename("Show Name - 01"), "Show Name - 01");
}

#[test]
fn sanitize_filename_is_idempotent() {
    // Running twice should produce the same output — sanitize
    // should never introduce characters that would get sanitized
    // on a second pass.
    let once = sanitize_filename("Show / 01 : Title");
    let twice = sanitize_filename(&once);
    assert_eq!(once, twice);
}

#[test]
fn sanitize_filename_returns_empty_for_empty_input() {
    assert_eq!(sanitize_filename(""), "");
}

#[test]
fn sanitize_filename_strips_filesystem_reserved_chars() {
    // Windows-reserved characters and path separators should not
    // appear in the output — Jellyfin / SMB / ext4 all choke on
    // different subsets and the safest policy is to replace them.
    let dirty = "Show: Name / 01 <test> |pipe| ?query";
    let clean = sanitize_filename(dirty);
    for bad in &[':', '/', '\\', '<', '>', '|', '?', '"', '*'] {
        assert!(
            !clean.contains(*bad),
            "cleaned name still contains {bad:?}: {clean}"
        );
    }
}
