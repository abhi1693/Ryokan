// Phase 1a foundation: nothing in production calls into this module yet — it
// is exercised only by unit tests until Phase 2b wires classify_post_download
// into `post_processing::import_torrent`. Remove this allow when that happens.
#![allow(dead_code)]

//! Layer 6 — directory / file-structure analysis.
//!
//! Scans the series root directory for BD-exclusive content that only
//! appears in Blu-ray releases. Catches cases where the individual video
//! file's filename is generic (`E01.mkv`) but the surrounding layout
//! gives the source away — a `BDMV/` folder next to it, a `Scans/`
//! sibling with cover art, creditless OP/ED files, etc.
//!
//! | Signal                                                  | Confidence |
//! |---------------------------------------------------------|------------|
//! | `BDMV/` dir, `.iso` file, or `Scans/` dir present       | 0.95       |
//! | NCOP / NCED creditless opening/ending file              | 0.90       |
//! | `Specials/`, `Extras/`, `Bonus/` subdirectory           | 0.80       |
//!
//! The public entry point reads directory entries from the filesystem,
//! but the rule evaluation is split into a pure function ([`scan_entries`])
//! that operates on an in-memory `Vec<DirItem>` so unit tests can feed
//! canned inputs without touching disk.
//!
//! This module does NOT fold evidence into a final decision. It emits
//! a bag of [`SourceEvidence`] for the caller to hand to
//! [`crate::services::source::aggregate`].

use std::path::Path;

use crate::services::source::{Source, SourceEvidence};

const ORIGIN: &str = "dir";

/// A directory entry as seen by the Layer 6 scanner. The public walker
/// populates one of these per `read_dir` result; tests construct them
/// directly to cover rule edge cases without tempdirs.
#[derive(Debug, Clone)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
}

impl DirItem {
    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
        }
    }

    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: true,
        }
    }
}

/// Walk the immediate children of `dir` and run the Layer 6 rules on them.
/// This only reads the top-level directory — it does not recurse. Empty
/// or unreadable directories return no evidence (Layer 6 is additive,
/// not required).
pub fn classify_dir(dir: &Path) -> Vec<SourceEvidence> {
    let items = match read_dir_items(dir) {
        Ok(items) => items,
        Err(_) => return Vec::new(),
    };
    scan_entries(&items)
}

/// Read the immediate children of `dir` as a `Vec<DirItem>`. Split out
/// from [`classify_dir`] so the pure scanner can be covered by unit
/// tests. Silently drops entries whose name isn't valid UTF-8.
fn read_dir_items(dir: &Path) -> std::io::Result<Vec<DirItem>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(DirItem { name, is_dir });
    }
    Ok(out)
}

/// Pure rule evaluator — takes a flat list of directory children and
/// emits zero or more pieces of evidence. Unit tests call this directly.
///
/// Rules fire independently and can overlap (a folder with both `BDMV/`
/// and `Specials/` produces two records). The aggregator de-duplicates
/// by source when summing.
pub fn scan_entries(items: &[DirItem]) -> Vec<SourceEvidence> {
    let mut out = Vec::new();

    // Rule: BD menu / disc artifacts → BluRay 0.95. These files only
    // exist when a release was sourced from the physical disc.
    for item in items {
        if item.is_dir && eq_ci(&item.name, "BDMV") {
            out.push(SourceEvidence::new(
                Source::BluRay,
                0.95,
                ORIGIN,
                "BDMV/ directory",
            ));
            break;
        }
    }
    for item in items {
        if !item.is_dir && has_extension_ci(&item.name, "iso") {
            out.push(SourceEvidence::new(
                Source::BluRay,
                0.95,
                ORIGIN,
                ".iso file",
            ));
            break;
        }
    }
    for item in items {
        if item.is_dir && eq_ci(&item.name, "Scans") {
            out.push(SourceEvidence::new(
                Source::BluRay,
                0.95,
                ORIGIN,
                "Scans/ directory",
            ));
            break;
        }
    }

    // Rule: NCOP / NCED files → BluRay 0.90. Creditless OP/ED versions
    // ship only on BD releases, typically named `NCOP1.mkv`, `NCED 01.mkv`,
    // `[Group] Series - NCOP01.mkv`, etc. Match the token anywhere in
    // the filename.
    for item in items {
        if item.is_dir {
            continue;
        }
        let upper = item.name.to_ascii_uppercase();
        if contains_token(&upper, "NCOP") || contains_token(&upper, "NCED") {
            out.push(SourceEvidence::new(
                Source::BluRay,
                0.90,
                ORIGIN,
                "NCOP / NCED creditless file",
            ));
            break;
        }
    }

    // Rule: Specials / Extras / Bonus subdirectory → BluRay 0.80. Weaker
    // than the BDMV rule because user-organized libraries sometimes have
    // a Specials folder even for streaming content, but still leans BD.
    for item in items {
        if !item.is_dir {
            continue;
        }
        if eq_ci(&item.name, "Specials")
            || eq_ci(&item.name, "Extras")
            || eq_ci(&item.name, "Bonus")
        {
            out.push(SourceEvidence::new(
                Source::BluRay,
                0.80,
                ORIGIN,
                format!("{} subdirectory", item.name),
            ));
            break;
        }
    }

    out
}

/// Case-insensitive string equality. Cheaper than allocating a lowercase
/// copy for single comparisons.
fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Case-insensitive extension check — `"foo.ISO"` matches `"iso"`.
fn has_extension_ci(name: &str, ext: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(ext))
        .unwrap_or(false)
}

/// Does `haystack` contain `needle` as a whole token — i.e. with a
/// non-alphanumeric boundary on either side? Used to distinguish
/// `NCOP01.mkv` (hit) from `SyncopationVol01.mkv` (miss). `haystack`
/// is assumed already uppercased; `needle` must be uppercase.
fn contains_token(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(needle) {
        let abs = start + idx;
        let before_ok = abs == 0
            || !haystack
                .as_bytes()
                .get(abs - 1)
                .copied()
                .map(|b| b.is_ascii_alphanumeric())
                .unwrap_or(false);
        let after_abs = abs + needle.len();
        let after_ok = after_abs == haystack.len()
            || !haystack
                .as_bytes()
                .get(after_abs)
                .copied()
                .map(|b| b.is_ascii_alphabetic())
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len();
        if start >= haystack.len() {
            break;
        }
    }
    false
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dir_has_no_evidence() {
        assert!(scan_entries(&[]).is_empty());
    }

    #[test]
    fn plain_episode_files_have_no_evidence() {
        let items = vec![
            DirItem::file("Series - S01E01.mkv"),
            DirItem::file("Series - S01E02.mkv"),
            DirItem::file("tvshow.nfo"),
            DirItem::file("poster.jpg"),
        ];
        assert!(scan_entries(&items).is_empty());
    }

    #[test]
    fn bdmv_dir_fires_disc_rule() {
        let items = vec![DirItem::dir("BDMV")];
        let evs = scan_entries(&items);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].source, Source::BluRay);
        assert!((evs[0].confidence - 0.95).abs() < 1e-4);
    }

    #[test]
    fn bdmv_case_insensitive() {
        let items = vec![DirItem::dir("bdmv")];
        assert!(!scan_entries(&items).is_empty());
    }

    #[test]
    fn iso_file_fires_disc_rule() {
        let items = vec![DirItem::file("Series Vol 1.iso")];
        let evs = scan_entries(&items);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].source, Source::BluRay);
    }

    #[test]
    fn iso_extension_case_insensitive() {
        let items = vec![DirItem::file("disc.ISO")];
        assert!(!scan_entries(&items).is_empty());
    }

    #[test]
    fn scans_dir_fires_disc_rule() {
        let items = vec![DirItem::dir("Scans")];
        let evs = scan_entries(&items);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].source, Source::BluRay);
    }

    #[test]
    fn ncop_file_fires_creditless_rule() {
        let items = vec![DirItem::file("[Group] Series - NCOP01 [1080p].mkv")];
        let evs = scan_entries(&items);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].source, Source::BluRay);
        assert!((evs[0].confidence - 0.90).abs() < 1e-4);
    }

    #[test]
    fn nced_file_fires_creditless_rule() {
        let items = vec![DirItem::file("Series NCED1.mkv")];
        assert!(!scan_entries(&items).is_empty());
    }

    #[test]
    fn ncop_token_requires_boundary() {
        // "SYNCOPATION" contains "NCOP" as a substring but not as a token.
        let items = vec![DirItem::file("SyncopationVol01.mkv")];
        assert!(scan_entries(&items).is_empty());
    }

    #[test]
    fn specials_subdir_fires_specials_rule() {
        let items = vec![DirItem::dir("Specials")];
        let evs = scan_entries(&items);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].source, Source::BluRay);
        assert!((evs[0].confidence - 0.80).abs() < 1e-4);
    }

    #[test]
    fn extras_subdir_also_fires() {
        let items = vec![DirItem::dir("Extras")];
        assert!(!scan_entries(&items).is_empty());
    }

    #[test]
    fn bonus_subdir_also_fires() {
        let items = vec![DirItem::dir("Bonus")];
        assert!(!scan_entries(&items).is_empty());
    }

    #[test]
    fn rules_stack_when_multiple_signals_present() {
        // A fully-laid-out BD dump: video files, NCOPs, Specials dir,
        // and Scans dir. All three distinct rules should fire.
        let items = vec![
            DirItem::file("Series - S01E01.mkv"),
            DirItem::file("Series - NCOP01.mkv"),
            DirItem::dir("Specials"),
            DirItem::dir("Scans"),
        ];
        let evs = scan_entries(&items);
        assert_eq!(evs.len(), 3);
        assert!(evs.iter().all(|e| e.source == Source::BluRay));
    }

    #[test]
    fn specials_as_filename_does_not_fire() {
        // Matching is on directories for the subdirectory rules — a file
        // named "Specials.mkv" should not fire the specials rule.
        let items = vec![DirItem::file("Specials.mkv")];
        assert!(scan_entries(&items).is_empty());
    }

    #[test]
    fn bdmv_as_filename_does_not_fire() {
        let items = vec![DirItem::file("BDMV.bak")];
        assert!(scan_entries(&items).is_empty());
    }

    #[test]
    fn integration_classify_dir_reads_real_tempdir() {
        // Quick end-to-end check that the public walker sees the files
        // it's supposed to. Uses std::env::temp_dir() + a uniquely-named
        // subdirectory so parallel tests don't clobber each other.
        let nonce = format!(
            "ryokan_source_dir_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let root = std::env::temp_dir().join(nonce);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("BDMV")).unwrap();
        std::fs::File::create(root.join("Series - S01E01.mkv")).unwrap();
        let evs = classify_dir(&root);
        assert!(evs.iter().any(|e| e.detail.contains("BDMV")));
        std::fs::remove_dir_all(&root).ok();
    }
}
