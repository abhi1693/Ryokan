//! Pluggable download-client abstraction.
//!
//! Ryokan addresses torrents by **v1 infohash** throughout, extracted
//! client-side from the magnet URL before any download client is
//! called. All BT clients key torrents by the same v1 infohash, so the
//! trait uses it as the canonical item ID. Each impl is responsible
//! for case-normalization (qBit/Deluge/Transmission want lowercase hex;
//! rtorrent wants uppercase).
//!
//! Trait contracts:
//!   - `info_hash: &str` parameters are **always lowercase hex** (40
//!     chars). Each impl case-converts internally for its wire format.
//!     Callers never case-munge.
//!   - `Result<_, String>` error type matches the existing project
//!     convention. Precludes caller-side retry policy based on error
//!     class; accepted until smart retry becomes a real feature.
//!   - All mutating operations are idempotent against repeated calls
//!     with the same `info_hash`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub mod qbittorrent;

/// Stub Deluge implementation. Exists during Phase 1 to compile-check
/// that the `DownloadClient` trait is object-safe and doesn't bake
/// qBit-specific assumptions into the method shapes. All methods
/// except `test()` and `sonarr_impl_name()` are `unimplemented!()` —
/// not wired into `AppState` or Settings UI. Phase 2 replaces with
/// the real impl.
pub mod deluge;

#[async_trait]
pub trait DownloadClient: Send + Sync {
    /// Test connection and return the client's version string.
    async fn test(&self) -> Result<String, String>;

    /// Add a torrent by magnet / HTTP `.torrent` URL. `info_hash` is
    /// the v1 infohash Ryokan pre-computed from the magnet; impls may
    /// use it for idempotency checks and addressing.
    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String>;

    /// Add a torrent and narrow it to a subset of its files. The `pick`
    /// callback receives the file names and returns the indices to
    /// keep (or `None` for a full grab). Each impl handles its own
    /// metadata-ready wait internally and must be idempotent: if a
    /// prior grab left this hash with priorities already set, re-narrow
    /// must not clobber user edits (use per-file `wanted` readback).
    ///
    /// The `pick` callback is `&mut dyn FnMut` rather than a generic
    /// `<F: FnMut>` to keep the trait object-safe — generic trait
    /// methods break `dyn DownloadClient`. Callers typically bind a
    /// closure and pass `&mut closure`.
    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        info_hash: &str,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String>;

    /// List torrents scoped to Ryokan's owned set only. Each impl
    /// defines "owned" — qBit uses `?category=`; Deluge the Label
    /// plugin; Transmission either native labels (4.x) or a
    /// save-path prefix; rtorrent the `custom1` field convention.
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String>;

    /// Files inside a torrent. Returns an empty `Vec` while metadata
    /// is still being fetched (each impl signals "not ready"
    /// differently — qBit returns 404; Transmission reports empty
    /// `files` with `metadataPercentComplete < 1.0`; Deluge has
    /// `has_metadata == false`). Trait contract normalizes all of
    /// these to "empty = not ready." See [`wait_for_files`] for the
    /// corresponding polling helper.
    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String>;

    async fn pause(&self, info_hash: &str) -> Result<(), String>;
    async fn resume(&self, info_hash: &str) -> Result<(), String>;
    async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), String>;

    /// Set per-file wanted/unwanted. Binary is sufficient — Ryokan
    /// only uses priority 0 (skip) and "normal" (include); qBit's
    /// higher priority levels are never written from Ryokan. Each
    /// impl maps `wanted: bool` to its native representation
    /// (qBit: `0` vs `1`; Deluge: `0` vs `4`; Transmission:
    /// `files-unwanted` vs `files-wanted`; rtorrent: `f.priority.set`
    /// 0 vs 1 *and* a mandatory `d.update_priorities` call).
    async fn set_file_wanted(
        &self,
        info_hash: &str,
        files: &[usize],
        wanted: bool,
    ) -> Result<(), String>;

    /// Sonarr-canonical implementation name for the
    /// `/api/v3/downloadclient` shim response. Values:
    /// `"QBittorrent" | "Deluge" | "Transmission" | "RTorrent"`.
    /// Distinct from the `active_client` discriminator
    /// (lowercase-snake: `"qbittorrent"` etc.).
    fn sonarr_impl_name(&self) -> &'static str;
}

/// Outcome of an [`DownloadClient::add_torrent`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// Client accepted the torrent fresh.
    Added,
    /// Client already had this infohash. qBit is silent about this;
    /// Transmission/Deluge/rtorrent surface a duplicate error that
    /// each impl catches and converts to this variant. Callers treat
    /// it as success — the torrent is in the client, which is the
    /// post-condition [`add_torrent`] promises.
    AlreadyPresent,
}

/// Outcome of an [`DownloadClient::add_torrent_with_file_filter`] call.
#[derive(Debug)]
pub enum SelectiveOutcome {
    /// Filter narrowed the torrent to specific files. Contains the
    /// kept file indices (always a strict subset of the file list).
    Filtered(Vec<usize>),
    /// No filter applied — the torrent is downloading all files.
    /// Used when the caller's `pick` returned `None`, when the pick
    /// matched every file (not a megapack after all), or when
    /// metadata fetch timed out and the impl resumed the already-
    /// added torrent unchanged instead of leaving it stuck paused.
    FullDownload,
}

/// A torrent as seen through the `DownloadClient` trait.
///
/// **JSON shape**: field layout matches the pre-refactor qBit
/// `Torrent` byte-for-byte so `templates/downloads.html` and the
/// frontend's state-label map (`templates/downloads.html:84-99`) keep
/// working without changes during Phase 1. `state` is the client-
/// native string (qBit: `"stalledUP"`, etc.); `state_kind` is the
/// normalized enum for internal state-matching but not serialized.
/// Phase 2+ may promote `state_kind` to the serialized shape when
/// the downloads UI generalizes.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct DownloadItem {
    pub hash: String,
    pub name: String,
    pub size: i64,
    pub progress: f64,
    pub dlspeed: i64,
    /// Client-native state string for UI display.
    pub state: String,
    pub category: String,
    pub eta: i64,
    #[serde(default)]
    pub save_path: String,
    /// Top-level path of the torrent's content (qBit ≥ 2.6.1 native;
    /// other impls compute from save_path + files' common prefix).
    /// Empty when metadata isn't ready yet.
    #[serde(default)]
    pub content_path: String,
    /// Normalized state for internal matching. Not serialized — the
    /// UI still reads the native `state` string during Phase 1.
    #[serde(skip, default)]
    pub state_kind: DownloadItemState,
}

/// One file inside a torrent.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct DownloadFile {
    /// Relative path of the file within the torrent (from save_path).
    pub name: String,
    pub size: i64,
    pub progress: f64,
    /// `true` when this file is actively being downloaded;
    /// `false` when the client was told to skip it. Used by the
    /// "already narrowed" idempotency check in
    /// `add_torrent_with_file_filter` so re-narrow doesn't clobber
    /// user edits from a prior grab.
    pub wanted: bool,
}

/// Normalized torrent state. 10 variants preserving the DL-vs-UL /
/// stalled / queued / checking distinctions the Downloads UI's
/// label map (`templates/downloads.html:84-99`) depends on. Each
/// impl maps its native state strings into this enum inside the
/// client's `list_scoped` implementation; `DownloadItem.state` keeps
/// the native string around for UI display so Phase 1 is a drop-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DownloadItemState {
    #[default]
    Downloading,
    DownloadingStalled,
    DownloadingQueued,
    CheckingDownload,
    Seeding,
    SeedingStalled,
    SeedingQueued,
    CheckingSeed,
    /// Paused while still incomplete.
    Paused,
    /// Paused/stopped after completion — treat as complete for
    /// post-processing purposes.
    PausedComplete,
    Errored,
}

impl DownloadItemState {
    /// Downloaded, verified, and safe to import. Post-processing's
    /// completion check bottoms out here.
    pub fn is_complete(self) -> bool {
        matches!(
            self,
            Self::Seeding
                | Self::SeedingStalled
                | Self::SeedingQueued
                | Self::CheckingSeed
                | Self::PausedComplete
        )
    }

    pub fn is_errored(self) -> bool {
        matches!(self, Self::Errored)
    }
}

/// Apply a remote → local path prefix rewrite. Used in post-processing
/// to translate paths from the download client's (possibly remote)
/// filesystem into paths Ryokan can read on its own host.
///
/// Modeled on Sonarr's Remote Path Mappings: when the download client
/// runs on a different host (seedbox), the `content_path` it reports
/// points at its own filesystem (e.g. `/downloads/…` inside the
/// seedbox). The user mounts that path locally via SSHFS/NFS/rclone
/// at a different prefix (e.g. `/mnt/seedbox/…`), and this function
/// performs the prefix swap before any filesystem op runs.
///
/// Both prefixes are trimmed of trailing `/` so `/downloads` and
/// `/downloads/` map identically. If `remote` or `local` is empty,
/// the input is returned unchanged — the "local client, no mapping"
/// case. If `remote` is set but the path doesn't begin with it,
/// return the path unchanged too: it's safer to surface a mismatch
/// than to silently rewrite a path that was never on the remote.
pub fn apply_remote_path_mapping(path: &str, remote: &str, local: &str) -> String {
    let r = remote.trim_end_matches('/');
    let l = local.trim_end_matches('/');
    if r.is_empty() || l.is_empty() {
        return path.to_string();
    }
    if let Some(rest) = path.strip_prefix(r) {
        // `rest` starts with `/` or is empty — either way, stitching
        // onto the trimmed local prefix produces the correct shape.
        format!("{l}{rest}")
    } else {
        path.to_string()
    }
}

/// Construct the concrete download-client impl dictated by the
/// config's `active_client` discriminator. Returns `None` if the
/// active client's credentials are empty (user hasn't configured it
/// yet) — the caller leaves `AppState.download_client` at `None` and
/// the grab path surfaces "Download client not configured" errors.
///
/// Single construction point: both startup init (`main.rs`) and
/// settings save (`handlers::settings`) go through this so the
/// "which impl do we pick" logic lives in one place and the arm for
/// each client ships alongside its `mod deluge` / `mod qbittorrent`
/// etc. as Phase 3+ clients land.
pub fn build_download_client(
    config: &crate::models::config::Config,
) -> Option<std::sync::Arc<dyn DownloadClient>> {
    match config.active_client.as_str() {
        "deluge" => {
            if config.deluge_url.is_empty() {
                return None;
            }
            Some(std::sync::Arc::new(deluge::DelugeClient::new(
                &config.deluge_url,
                &config.deluge_password,
                &config.deluge_label,
            )))
        }
        // "qbittorrent" or any unknown value — qBit is the safe
        // default to preserve pre-Phase-2 behavior for unrecognized
        // discriminators.
        _ => {
            if config.qbit_url.is_empty() {
                return None;
            }
            Some(std::sync::Arc::new(qbittorrent::QbitClient::new(
                &config.qbit_url,
                &config.qbit_user,
                &config.qbit_pass,
                &config.qbit_category,
            )))
        }
    }
}

/// Poll `get_files` until non-empty or `timeout` elapses. 500ms
/// initial interval, doubling up to a 2s cap. Used by callers that
/// need the file list before proceeding (e.g. the 180s background
/// auto-expand wait in `handlers::library::search`). Impls that need
/// a wait internally (inside `add_torrent_with_file_filter`) may use
/// this or write their own — it's just a convenience over the trait
/// method.
pub async fn wait_for_files(
    client: &dyn DownloadClient,
    info_hash: &str,
    timeout: Duration,
) -> Result<Vec<DownloadFile>, String> {
    let start = Instant::now();
    let mut delay = Duration::from_millis(500);
    loop {
        match client.get_files(info_hash).await {
            Ok(files) if !files.is_empty() => return Ok(files),
            Ok(_) => {}
            Err(e) => {
                if start.elapsed() >= timeout {
                    return Err(format!("metadata fetch error after {:?}: {}", timeout, e));
                }
            }
        }
        if start.elapsed() >= timeout {
            return Err(format!("metadata fetch timed out after {:?}", timeout));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

/// Compute a torrent's `content_path` from its `save_path` and file
/// list, for clients (Deluge/Transmission/rtorrent) that don't expose
/// it natively the way qBit ≥ 2.6.1 does. Handles the three cases:
///
///   1. **Single-file torrent** (`files.len() == 1`, no `/` in the
///      name): `save_path + "/" + files[0].name`. Points at the
///      file itself.
///   2. **Multi-file with wrapping directory** (all `files[i].name`
///      share a non-empty prefix ending in `/`):
///      `save_path + "/" + common_prefix_dir`. Points at the folder.
///   3. **Multi-file dumped at save root** (no common prefix):
///      `content_path == save_path`.
///
/// Returns an empty string if `files` is empty (metadata not yet
/// known) — caller should check and retry.
///
/// Phase 1: unused (qBit impl uses its native `content_path` field).
/// Phase 2+ impls will call this to produce a client-agnostic path.
#[allow(dead_code)]
pub fn compute_content_path(save_path: &str, files: &[DownloadFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let save = save_path.trim_end_matches('/');
    if files.len() == 1 && !files[0].name.contains('/') {
        return format!("{save}/{}", files[0].name);
    }
    let first = &files[0].name;
    let Some(slash_idx) = first.find('/') else {
        return save.to_string();
    };
    let candidate = &first[..=slash_idx];
    if files.iter().all(|f| f.name.starts_with(candidate)) {
        return format!("{save}/{}", candidate.trim_end_matches('/'));
    }
    save.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str) -> DownloadFile {
        DownloadFile {
            name: name.to_string(),
            size: 0,
            progress: 0.0,
            wanted: true,
        }
    }

    #[test]
    fn content_path_single_file_points_at_file() {
        let files = vec![f("release.mkv")];
        assert_eq!(
            compute_content_path("/downloads", &files),
            "/downloads/release.mkv"
        );
    }

    #[test]
    fn content_path_multi_file_with_wrapper_points_at_folder() {
        let files = vec![
            f("[Group] Show/01.mkv"),
            f("[Group] Show/02.mkv"),
            f("[Group] Show/03.mkv"),
        ];
        assert_eq!(
            compute_content_path("/downloads", &files),
            "/downloads/[Group] Show"
        );
    }

    #[test]
    fn content_path_multi_file_dumped_at_root_is_save_path() {
        let files = vec![f("01.mkv"), f("02.mkv"), f("03.mkv")];
        assert_eq!(compute_content_path("/downloads", &files), "/downloads");
    }

    #[test]
    fn content_path_mixed_folders_no_common_prefix() {
        let files = vec![f("folder_a/01.mkv"), f("folder_b/02.mkv"), f("bare.mkv")];
        assert_eq!(compute_content_path("/downloads", &files), "/downloads");
    }

    #[test]
    fn content_path_normalizes_trailing_slash_on_save_path() {
        let files = vec![f("release.mkv")];
        assert_eq!(
            compute_content_path("/downloads/", &files),
            "/downloads/release.mkv"
        );
    }

    #[test]
    fn content_path_empty_files_returns_empty() {
        assert_eq!(compute_content_path("/downloads", &[]), "");
    }

    #[test]
    fn remote_path_mapping_rewrites_matching_prefix() {
        assert_eq!(
            apply_remote_path_mapping("/downloads/anime/file.mkv", "/downloads", "/mnt/seedbox"),
            "/mnt/seedbox/anime/file.mkv"
        );
    }

    #[test]
    fn remote_path_mapping_trims_trailing_slashes_on_prefixes() {
        // Trailing-slash normalization on both sides should produce
        // identical output — prevents the /downloads vs /downloads/
        // foot-gun that bites every Sonarr remote-path setup.
        assert_eq!(
            apply_remote_path_mapping("/downloads/x.mkv", "/downloads/", "/mnt/seedbox/"),
            "/mnt/seedbox/x.mkv"
        );
        assert_eq!(
            apply_remote_path_mapping("/downloads/x.mkv", "/downloads", "/mnt/seedbox"),
            "/mnt/seedbox/x.mkv"
        );
    }

    #[test]
    fn remote_path_mapping_empty_prefixes_pass_through() {
        // No mapping configured = no rewrite. The "local client"
        // case; both prefixes empty means identity.
        assert_eq!(
            apply_remote_path_mapping("/downloads/x.mkv", "", ""),
            "/downloads/x.mkv"
        );
    }

    #[test]
    fn remote_path_mapping_non_matching_prefix_unchanged() {
        // If the path isn't under the configured remote prefix,
        // don't silently rewrite — could indicate user mis-config.
        assert_eq!(
            apply_remote_path_mapping("/other/path.mkv", "/downloads", "/mnt/seedbox"),
            "/other/path.mkv"
        );
    }

    #[test]
    fn state_is_complete_catches_all_seed_variants() {
        use DownloadItemState::*;
        assert!(Seeding.is_complete());
        assert!(SeedingStalled.is_complete());
        assert!(SeedingQueued.is_complete());
        assert!(CheckingSeed.is_complete());
        assert!(PausedComplete.is_complete());
    }

    #[test]
    fn state_is_complete_rejects_download_variants() {
        use DownloadItemState::*;
        assert!(!Downloading.is_complete());
        assert!(!DownloadingStalled.is_complete());
        assert!(!DownloadingQueued.is_complete());
        assert!(!CheckingDownload.is_complete());
        assert!(!Paused.is_complete());
        assert!(!Errored.is_complete());
    }
}
