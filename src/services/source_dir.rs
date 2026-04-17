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
//! The disc-marker and creditless-file rules walk two levels deep so they
//! still fire when the release is nested one folder below the series
//! root — e.g. `Series Name/[Group] Series Name/BDMV/` or
//! `Series Name/Vol 1/BDMV/`. Without this the rules were blind to the
//! common layout where a group's release folder sits inside the tracked
//! series directory. The `Specials/Extras/Bonus` rule stays top-level
//! only because it's weaker and more prone to false positives at depth.
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

use crate::services::source::{contains_word, Origin, Source, SourceEvidence};

const ORIGIN: Origin = Origin::Dir;

/// A directory entry as seen by the Layer 6 scanner. The public walker
/// populates one of these per `read_dir` result; tests construct them
/// directly to cover rule edge cases without tempdirs.
#[derive(Debug, Clone)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    /// 0 for immediate children of the series root, 1 for grandchildren.
    /// Rules consult this to decide whether the signal they emit should
    /// only fire at the top level (weak/ambiguous rules) or whether it's
    /// OK to let the rule match on a nested release folder (strong disc
    /// markers).
    pub depth: u8,
}

#[cfg(test)]
impl DirItem {
    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
            depth: 0,
        }
    }

    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: true,
            depth: 0,
        }
    }

    pub fn nested_file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
            depth: 1,
        }
    }

    pub fn nested_dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: true,
            depth: 1,
        }
    }
}

/// Walk `dir` (and its immediate subdirectories) and run the Layer 6
/// rules on the combined listing. Empty or unreadable directories return
/// no evidence (Layer 6 is additive, not required).
pub fn classify_dir(dir: &Path) -> Vec<SourceEvidence> {
    let items = match read_dir_items(dir) {
        Ok(items) => items,
        Err(_) => return Vec::new(),
    };
    scan_entries(&items)
}

/// Maximum number of entries to read per subdirectory during the
/// grandchild walk. Caps the cost of scanning pathologically large
/// directories (cache folders, user-organized libraries with hundreds
/// of files per season) — if the release folder really contains
/// hundreds of items we'd have already caught the BD markers well
/// before this limit bites.
const MAX_NESTED_ENTRIES: usize = 200;

/// Subdirectories we never recurse into, regardless of depth. These are
/// either tool metadata (`.DS_Store` etc.), symlink roots that could
/// introduce cycles, or directories we already know fire their own rule
/// at depth 0 (Scans/Specials/etc.) so descending into them just wastes
/// I/O.
fn is_skipped_subdir(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "scans"
            | "specials"
            | "extras"
            | "bonus"
            | "@eadir"
            | "lost+found"
            | "__macosx"
    )
}

/// Read the children of `dir` as a `Vec<DirItem>`. Walks up to two levels
/// (the top-level entries at depth 0, plus the immediate contents of
/// each top-level subdirectory at depth 1). Split out from
/// [`classify_dir`] so the pure scanner can be covered by unit tests.
/// Silently drops entries whose name isn't valid UTF-8.
fn read_dir_items(dir: &Path) -> std::io::Result<Vec<DirItem>> {
    let mut out = Vec::new();
    let mut top_dirs = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            top_dirs.push((name.clone(), entry.path()));
        }
        out.push(DirItem { name, is_dir, depth: 0 });
    }

    // Second pass: walk one level into each top-level subdirectory.
    // Read_dir failures on a single child are swallowed — the layer is
    // additive, a missing grandchild listing just means we fall back to
    // whatever the top-level listing already told us.
    for (parent_name, parent_path) in top_dirs {
        if is_skipped_subdir(&parent_name) {
            continue;
        }
        let Ok(nested) = std::fs::read_dir(&parent_path) else {
            continue;
        };
        // `.take` caps the nested walk at MAX_NESTED_ENTRIES without
        // needing a manual counter — clippy likes this better and the
        // behavior is identical: stop after N items even if there are
        // more on disk.
        for entry in nested.take(MAX_NESTED_ENTRIES) {
            let Ok(entry) = entry else { continue };
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(DirItem { name, is_dir, depth: 1 });
        }
    }

    Ok(out)
}

/// Pure rule evaluator — takes a flat list of directory children and
/// emits zero or more pieces of evidence. Unit tests call this directly.
///
/// Rules fire independently and can overlap (a folder with both `BDMV/`
/// and `Specials/` produces two records). The aggregator de-duplicates
/// by source when summing.
///
/// Strong rules (BDMV, .iso, Scans, NCOP/NCED) accept items at any
/// depth so a nested release folder still lights them up. The weaker
/// Specials/Extras/Bonus rule only fires on top-level directories
/// (`depth == 0`) — seeing a `Specials` folder one level deep inside a
/// group release folder is too noisy to count as a BD signal on its
/// own.
pub fn scan_entries(items: &[DirItem]) -> Vec<SourceEvidence> {
    let mut out = Vec::new();

    // Rule: BD menu / disc artifacts → BluRay 0.95. These files only
    // exist when a release was sourced from the physical disc. Accept
    // matches at any depth so nested release folders still fire.
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
    // the filename; `contains_word` handles the digit/punctuation boundary
    // so `NCOP01` hits but `Syncopation` doesn't.
    for item in items {
        if item.is_dir {
            continue;
        }
        if contains_word(&item.name, "NCOP") || contains_word(&item.name, "NCED") {
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
    // Top-level only: a `Specials` folder one level deeper (inside a
    // group release dir) could just as easily mean "the group organized
    // the stream rips this way."
    for item in items {
        if !item.is_dir || item.depth != 0 {
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

    #[test]
    fn nested_bdmv_fires_disc_rule() {
        // Common layout: `Series Name/[Group] Series Name (BD 1080p)/BDMV/`
        // — the series root only has a single child dir, and the disc
        // markers live one level deeper. The rule must still fire or
        // the BD signal is invisible to the classification pipeline.
        let items = vec![
            DirItem::dir("[Group] Series Name (BD 1080p)"),
            DirItem::nested_dir("BDMV"),
            DirItem::nested_dir("CERTIFICATE"),
        ];
        let evs = scan_entries(&items);
        assert!(evs.iter().any(|e| e.source == Source::BluRay && e.detail.contains("BDMV")));
    }

    #[test]
    fn nested_iso_fires_disc_rule() {
        // `Series/Vol 1/disc.iso` layout.
        let items = vec![
            DirItem::dir("Vol 1"),
            DirItem::nested_file("disc.iso"),
        ];
        let evs = scan_entries(&items);
        assert!(evs.iter().any(|e| e.source == Source::BluRay && e.detail.contains("iso")));
    }

    #[test]
    fn nested_ncop_fires_creditless_rule() {
        let items = vec![
            DirItem::dir("[Group] Series Name (BD 1080p)"),
            DirItem::nested_file("Series - NCOP01.mkv"),
            DirItem::nested_file("Series - S01E01.mkv"),
        ];
        let evs = scan_entries(&items);
        assert!(evs.iter().any(|e| e.detail.contains("NCOP")));
    }

    #[test]
    fn nested_specials_dir_does_not_fire_top_level_rule() {
        // `Series/[Group]/Specials/` — the Specials folder is one level
        // too deep to be a reliable signal. The weaker Specials rule
        // must NOT fire.
        let items = vec![
            DirItem::dir("[Group] Series Name"),
            DirItem::nested_dir("Specials"),
        ];
        assert!(scan_entries(&items).is_empty());
    }

    #[test]
    fn top_level_specials_still_fires() {
        // Regression-guard for the depth gating: a user-organized root
        // with a top-level Specials directory keeps the old behavior.
        let items = vec![
            DirItem::file("Series - S01E01.mkv"),
            DirItem::dir("Specials"),
        ];
        let evs = scan_entries(&items);
        assert!(evs.iter().any(|e| e.detail.contains("Specials")));
    }

    #[test]
    fn integration_nested_bdmv_walked() {
        // End-to-end: a BDMV dir one level inside a group folder should
        // still be surfaced by the public walker.
        let nonce = format!(
            "ryokan_source_dir_nested_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let root = std::env::temp_dir().join(nonce);
        let group_dir = root.join("[Group] Series Name (BD 1080p)");
        std::fs::create_dir_all(group_dir.join("BDMV")).unwrap();
        std::fs::File::create(group_dir.join("Series - S01E01.mkv")).unwrap();
        let evs = classify_dir(&root);
        assert!(evs.iter().any(|e| e.detail.contains("BDMV")));
        std::fs::remove_dir_all(&root).ok();
    }
}
