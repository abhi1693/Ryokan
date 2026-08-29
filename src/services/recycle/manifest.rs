//! On-disk manifest for one recycle-bin entry (#123).
//!
//! The manifest is the single source of truth for restore. Encoding the
//! original path into the entry's directory name was rejected because it
//! fails on long paths and on characters the recycle filesystem can't
//! carry; a JSON sidecar is portable and extensible (future fields land
//! without a layout change).

use serde::{Deserialize, Serialize};

/// What a recycle entry holds. Drives both the companion-file sweep at
/// recycle time and the restore strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecycleKind {
    /// A single episode video plus its companions (`.nfo`, subtitles,
    /// thumbnails) discovered next to it.
    Episode,
    /// An entire series folder moved as one unit.
    SeriesFolder,
}

impl RecycleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RecycleKind::Episode => "episode",
            RecycleKind::SeriesFolder => "series_folder",
        }
    }
}

/// `manifest.json` inside every `<recycle_bin>/<YYYY-MM-DD>/<entry_id>/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecycleManifest {
    pub kind: RecycleKind,
    /// `None` when the series row was already gone at recycle time (or
    /// the caller had no row to hand us). Restore never depends on it.
    pub series_id: Option<i64>,
    /// Carried in the manifest so the recycle list renders correctly
    /// after the series row is deleted.
    pub series_title: String,
    /// Absolute path of the original file (`Episode`) or directory
    /// (`SeriesFolder`). Restore reconstructs from this.
    pub original_path: String,
    /// Unix seconds.
    pub recycled_at: i64,
    /// Total bytes across every file in the entry.
    pub size_bytes: u64,
    /// Basenames inside the entry directory, main file last.
    pub files: Vec<String>,
}

pub const MANIFEST_FILE: &str = "manifest.json";
