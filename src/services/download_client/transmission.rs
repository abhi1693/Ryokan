//! Transmission implementation of [`DownloadClient`]. Speaks the
//! Transmission JSON-RPC API at `POST <base_url>/transmission/rpc`.
//!
//! Transmission-specific quirks worth flagging (verified against
//! Transmission 4.1.1 rpc-version 19, live-probed 2026-04-21):
//!   - **CSRF session handshake**: every first request is rejected
//!     with HTTP 409 and a `X-Transmission-Session-Id` response
//!     header. The client must echo that header on every subsequent
//!     request or the server re-409s. The session ID rotates when
//!     the daemon restarts, so any mid-stream 409 means "re-capture
//!     and retry once." The [`send`](TransmissionClient::send) helper
//!     handles both first-call and mid-stream 409s transparently.
//!   - **Auth is HTTP Basic, not RPC-level**: `transmission-daemon`
//!     accepts credentials via the standard Authorization header;
//!     there's no `auth.login` method. Wrong creds surface as 401
//!     rather than an RPC envelope error.
//!   - **Native `labels` in 4.x**: `torrent-add`/`torrent-set` accept
//!     a `labels: [String]` parameter and `torrent-get` returns it
//!     populated. Ryokan scopes by filtering `labels.contains(self.label)`
//!     client-side — the RPC has no server-side label filter.
//!   - **File-selection scale is 0 / 1 (unwanted / wanted)**, not
//!     qBit's priority scale. `torrent-set` takes two parallel arrays:
//!     `files-wanted: [idx]` and `files-unwanted: [idx]`. Priority is
//!     a separate axis (`priority-high`/`-normal`/`-low`) that Ryokan
//!     doesn't touch — only the wanted/unwanted flag.
//!   - **Duplicate-add surfaces as `torrent-duplicate`** inside the
//!     success envelope (`result: "success"`), not as an error. No
//!     message-parsing like Deluge needs; the `torrent-duplicate` key
//!     presence is the signal.
//!   - **Completion is percentDone, not isFinished**: `isFinished`
//!     means "hit seed ratio/time target" (user-defined stop
//!     condition), not "download complete." Ryokan uses `percentDone
//!     >= 1.0` combined with the status code to derive
//!     `DownloadItemState::is_complete()`.
//!   - **Status codes are small ints** (0..=6): 0=Stopped, 1=Queued-
//!     to-verify, 2=Verifying, 3=Queued-to-download, 4=Downloading,
//!     5=Queued-to-seed, 6=Seeding. Combined with `percentDone` to
//!     decide Paused vs PausedComplete.
//!   - **Metadata-ready signal**: pre-metadata magnets return
//!     `files: []` and `metadataPercentComplete < 1.0`. Trait contract
//!     says `get_files` returns empty = "not ready," same as Deluge.

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use super::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, DownloadItemState, SelectiveOutcome,
};

const SESSION_HEADER: &str = "X-Transmission-Session-Id";

/// Same 10s budget as qBit/Deluge so selective-narrowing latency is
/// consistent across impls.
const METADATA_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct TransmissionClient {
    base_url: String,
    user: String,
    password: String,
    label: String,
    http: Client,
    /// Current CSRF session ID. `None` before the first successful
    /// request; Some(_) after the handshake. Captured from the 409
    /// response's `X-Transmission-Session-Id` header and echoed on
    /// subsequent requests. Rotates when the daemon restarts, so
    /// any mid-stream 409 clears and re-captures it.
    session_id: Arc<RwLock<Option<String>>>,
}

#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    method: &'a str,
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    result: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize, Default)]
struct TxRawTorrent {
    #[serde(default, rename = "hashString")]
    hash_string: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "totalSize")]
    total_size: i64,
    #[serde(default, rename = "percentDone")]
    percent_done: f64,
    #[serde(default, rename = "rateDownload")]
    rate_download: i64,
    #[serde(default)]
    status: i32,
    #[serde(default)]
    eta: i64,
    #[serde(default, rename = "downloadDir")]
    download_dir: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default, rename = "isStalled")]
    is_stalled: bool,
    #[serde(default, rename = "errorString")]
    error_string: String,
    #[serde(default)]
    files: Vec<TxRawFile>,
    #[serde(default, rename = "fileStats")]
    file_stats: Vec<TxRawFileStat>,
}

#[derive(Debug, Deserialize, Default)]
struct TxRawFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    length: i64,
    #[serde(default, rename = "bytesCompleted")]
    bytes_completed: i64,
}

#[derive(Debug, Deserialize, Default)]
struct TxRawFileStat {
    #[serde(default)]
    wanted: bool,
}

impl TransmissionClient {
    pub fn new(base_url: &str, user: &str, password: &str, label: &str) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: normalize_base_url(base_url),
            user: user.to_string(),
            password: password.to_string(),
            label: if label.is_empty() {
                "ryokan".to_string()
            } else {
                label.to_string()
            },
            http,
            session_id: Arc::new(RwLock::new(None)),
        }
    }

    fn rpc_url(&self) -> String {
        format!("{}/transmission/rpc", self.base_url)
    }

    /// JSON-RPC round-trip with transparent CSRF session handling.
    /// Captures `X-Transmission-Session-Id` from 409 responses and
    /// retries once. Returns the raw `arguments` value on success.
    async fn send(&self, method: &str, arguments: Value) -> Result<Value, String> {
        let body = RpcRequest { method, arguments };
        let mut attempt = 0;
        loop {
            attempt += 1;

            let mut req = self.http.post(self.rpc_url()).json(&body);
            if !self.user.is_empty() || !self.password.is_empty() {
                req = req.basic_auth(&self.user, Some(&self.password));
            }
            if let Some(sid) = self.session_id.read().await.clone() {
                req = req.header(SESSION_HEADER, sid);
            }

            let resp = req
                .send()
                .await
                .map_err(|e| format!("Transmission request failed: {e}"))?;

            if resp.status() == StatusCode::CONFLICT {
                // CSRF: capture new session-id and retry once.
                let new_sid = resp
                    .headers()
                    .get(SESSION_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let Some(new_sid) = new_sid else {
                    return Err(
                        "Transmission returned 409 without X-Transmission-Session-Id header".into(),
                    );
                };
                *self.session_id.write().await = Some(new_sid);
                if attempt >= 2 {
                    return Err("Transmission returned 409 after session-id retry".into());
                }
                continue;
            }

            if resp.status() == StatusCode::UNAUTHORIZED {
                return Err("Transmission auth failed: check username/password".into());
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("Transmission HTTP {status}: {}", body.trim()));
            }

            let parsed: RpcResponse = resp
                .json()
                .await
                .map_err(|e| format!("Transmission response parse failed: {e}"))?;

            if parsed.result != "success" {
                return Err(parsed.result);
            }
            return Ok(parsed.arguments);
        }
    }

    async fn get_torrent(&self, info_hash: &str, fields: &[&str]) -> Result<TxRawTorrent, String> {
        let hash_lc = info_hash.to_ascii_lowercase();
        let raw = self
            .send("torrent-get", json!({"ids": [hash_lc], "fields": fields}))
            .await?;
        let list: Vec<TxRawTorrent> =
            serde_json::from_value(raw.get("torrents").cloned().unwrap_or(Value::Array(vec![])))
                .map_err(|e| format!("Transmission torrent-get parse failed: {e}"))?;
        list.into_iter()
            .next()
            .ok_or_else(|| format!("Transmission: torrent {} not found", info_hash))
    }
}

#[async_trait]
impl DownloadClient for TransmissionClient {
    async fn test(&self) -> Result<String, String> {
        let args = self
            .send("session-get", json!({"fields": ["version", "rpc-version"]}))
            .await?;
        let version = args
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(version)
    }

    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        let args = self
            .send(
                "torrent-add",
                json!({
                    "filename": url,
                    "labels": [self.label],
                }),
            )
            .await?;

        // Transmission returns either `torrent-added` or
        // `torrent-duplicate` under the `arguments` object. Both carry
        // `{id, name, hashString}`; presence of `torrent-duplicate` is
        // the sole duplicate-add signal.
        if args.get("torrent-duplicate").is_some() {
            // Duplicate-add doesn't re-apply labels — explicitly set
            // our scoping label so a user-added-then-re-grabbed torrent
            // becomes visible to `list_scoped`. Match the Deluge impl's
            // "adopt existing torrent" semantics.
            if !info_hash.is_empty() {
                let _ = self
                    .send(
                        "torrent-set",
                        json!({
                            "ids": [info_hash.to_ascii_lowercase()],
                            "labels": [self.label],
                        }),
                    )
                    .await;
            }
            return Ok(AddOutcome::AlreadyPresent);
        }
        Ok(AddOutcome::Added)
    }

    async fn add_torrent_with_file_filter(
        &self,
        url: &str,
        info_hash: &str,
        pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        if info_hash.is_empty() {
            return Err("Transmission selective download requires a known info hash".into());
        }
        let hash_lc = info_hash.to_ascii_lowercase();

        // Add paused so we can apply the filter before the torrent
        // starts consuming bandwidth on skipped files. If add fails
        // duplicate, that's fine — the existing torrent may already be
        // running; we apply the filter either way.
        let add_args = self
            .send(
                "torrent-add",
                json!({
                    "filename": url,
                    "labels": [self.label],
                    "paused": true,
                }),
            )
            .await?;
        let was_duplicate = add_args.get("torrent-duplicate").is_some();

        // Poll for metadata readiness.
        let start = Instant::now();
        let mut delay = Duration::from_millis(500);
        let (files, _file_stats): (Vec<TxRawFile>, Vec<TxRawFileStat>) = loop {
            let torrent = self.get_torrent(&hash_lc, &["files", "fileStats"]).await?;
            if !torrent.files.is_empty() {
                break (torrent.files, torrent.file_stats);
            }
            if start.elapsed() >= METADATA_WAIT_TIMEOUT {
                // Start the torrent unfiltered — same fallback as qBit/Deluge.
                let _ = self.send("torrent-start", json!({"ids": [hash_lc]})).await;
                return Ok(SelectiveOutcome::FullDownload);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        };

        let names: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
        let keep_indices = match pick(&names) {
            Some(ids) if !ids.is_empty() && ids.len() < files.len() => ids,
            _ => {
                // Caller said "keep all" (or matched all) — start and
                // return FullDownload.
                let _ = self.send("torrent-start", json!({"ids": [hash_lc]})).await;
                return Ok(SelectiveOutcome::FullDownload);
            }
        };

        // Detect prior narrowing (re-grab case): if any existing file
        // is marked unwanted, merge additively — only flip new files
        // from unwanted → wanted; leave existing wanted files alone.
        // Matches the Deluge impl's re-grab idempotency.
        let existing = self.get_torrent(&hash_lc, &["fileStats"]).await.ok();
        let already_narrowed = existing
            .as_ref()
            .map(|t| t.file_stats.iter().any(|f| !f.wanted))
            .unwrap_or(false);

        let unwanted: Vec<usize> = if already_narrowed {
            // Nothing to do for files already unwanted — only extend
            // the wanted set. Compute the indices NOT in keep_indices
            // and intersect with currently-unwanted; anything not in
            // that intersection stays as-is (no disturb).
            Vec::new()
        } else {
            (0..files.len())
                .filter(|i| !keep_indices.contains(i))
                .collect()
        };

        let wanted_patch: Vec<usize> = if already_narrowed {
            keep_indices.clone()
        } else {
            Vec::new()
        };

        let mut patch = json!({"ids": [hash_lc]});
        if let Some(obj) = patch.as_object_mut() {
            if !unwanted.is_empty() {
                obj.insert("files-unwanted".into(), json!(unwanted));
            }
            if !wanted_patch.is_empty() {
                obj.insert("files-wanted".into(), json!(wanted_patch));
            }
        }
        if patch.as_object().map(|o| o.len()).unwrap_or(0) > 1 {
            self.send("torrent-set", patch).await?;
        }

        // Unpause unless we were a duplicate of an already-running
        // torrent (the torrent-add paused:true flag is ignored for
        // duplicates, so starting it is a no-op but still safe).
        let _ = self.send("torrent-start", json!({"ids": [hash_lc]})).await;
        let _ = was_duplicate;

        Ok(SelectiveOutcome::Filtered(keep_indices))
    }

    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        // torrent-get returns all torrents; filter client-side by
        // label. The RPC has no server-side label filter.
        let fields = [
            "id",
            "hashString",
            "name",
            "totalSize",
            "percentDone",
            "rateDownload",
            "status",
            "eta",
            "downloadDir",
            "labels",
            "isStalled",
            "errorString",
            "files",
            "fileStats",
        ];
        let args = self.send("torrent-get", json!({"fields": fields})).await?;
        let list: Vec<TxRawTorrent> = serde_json::from_value(
            args.get("torrents")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )
        .map_err(|e| format!("Transmission list_scoped parse failed: {e}"))?;

        Ok(list
            .into_iter()
            .filter(|t| t.labels.iter().any(|l| l == &self.label))
            .map(to_download_item)
            .collect())
    }

    async fn get_files(&self, info_hash: &str) -> Result<Vec<DownloadFile>, String> {
        let torrent = self.get_torrent(info_hash, &["files", "fileStats"]).await?;
        Ok(to_download_files(&torrent))
    }

    async fn pause(&self, info_hash: &str) -> Result<(), String> {
        self.send(
            "torrent-stop",
            json!({"ids": [info_hash.to_ascii_lowercase()]}),
        )
        .await?;
        Ok(())
    }

    async fn resume(&self, info_hash: &str) -> Result<(), String> {
        self.send(
            "torrent-start",
            json!({"ids": [info_hash.to_ascii_lowercase()]}),
        )
        .await?;
        Ok(())
    }

    async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), String> {
        self.send(
            "torrent-remove",
            json!({
                "ids": [info_hash.to_ascii_lowercase()],
                "delete-local-data": delete_files,
            }),
        )
        .await?;
        Ok(())
    }

    async fn set_file_wanted(
        &self,
        info_hash: &str,
        files: &[usize],
        wanted: bool,
    ) -> Result<(), String> {
        let hash_lc = info_hash.to_ascii_lowercase();
        let key = if wanted {
            "files-wanted"
        } else {
            "files-unwanted"
        };
        self.send(
            "torrent-set",
            json!({
                "ids": [hash_lc],
                key: files,
            }),
        )
        .await?;
        Ok(())
    }

    fn sonarr_impl_name(&self) -> &'static str {
        "Transmission"
    }
}

fn to_download_item(raw: TxRawTorrent) -> DownloadItem {
    let state_kind = map_tx_state(&raw);
    let files_view = to_download_files(&raw);
    let content_path = super::compute_content_path(&raw.download_dir, &files_view);
    // Preserve the numeric status as the UI state string — no
    // established Transmission state-label vocabulary to match against,
    // and the Downloads UI falls through to a sensible default.
    let state_str = match raw.status {
        0 if raw.percent_done >= 1.0 => "Stopped (complete)".to_string(),
        0 => "Stopped".to_string(),
        1 => "Queued (verify)".to_string(),
        2 => "Verifying".to_string(),
        3 => "Queued (download)".to_string(),
        4 if raw.is_stalled => "Stalled (DL)".to_string(),
        4 => "Downloading".to_string(),
        5 => "Queued (seed)".to_string(),
        6 if raw.is_stalled => "Stalled (UL)".to_string(),
        6 => "Seeding".to_string(),
        _ => format!("status {}", raw.status),
    };
    DownloadItem {
        hash: raw.hash_string,
        name: raw.name,
        size: raw.total_size,
        progress: raw.percent_done,
        dlspeed: raw.rate_download,
        state: state_str,
        category: raw.labels.first().cloned().unwrap_or_default(),
        eta: raw.eta,
        save_path: raw.download_dir,
        content_path,
        state_kind,
    }
}

fn to_download_files(raw: &TxRawTorrent) -> Vec<DownloadFile> {
    raw.files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let wanted = raw.file_stats.get(i).map(|s| s.wanted).unwrap_or(true);
            let progress = if f.length > 0 {
                (f.bytes_completed as f64 / f.length as f64).min(1.0)
            } else {
                0.0
            };
            DownloadFile {
                name: f.name.clone(),
                size: f.length,
                progress,
                wanted,
            }
        })
        .collect()
}

/// Map Transmission's (status, percentDone, isStalled, errorString)
/// tuple to Ryokan's 10-variant enum. Matches the Deluge impl's
/// "complete-means-downloaded-not-seed-goal-hit" semantics.
fn map_tx_state(raw: &TxRawTorrent) -> DownloadItemState {
    use DownloadItemState::*;
    if !raw.error_string.is_empty() {
        return Errored;
    }
    let is_complete = raw.percent_done >= 1.0;
    match raw.status {
        0 if is_complete => PausedComplete,
        0 => Paused,
        1 if is_complete => CheckingSeed,
        1 => CheckingDownload,
        2 if is_complete => CheckingSeed,
        2 => CheckingDownload,
        3 => DownloadingQueued,
        4 if raw.is_stalled => DownloadingStalled,
        4 => Downloading,
        5 => SeedingQueued,
        6 if raw.is_stalled => SeedingStalled,
        6 => Seeding,
        _ => Downloading,
    }
}

fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
    // Same local-vs-public heuristic as the qBit/Deluge impls.
    let lower = trimmed.to_ascii_lowercase();
    let is_local = lower.starts_with("localhost")
        || lower.starts_with("127.")
        || lower.starts_with("10.")
        || lower.starts_with("192.168.")
        || lower.starts_with("172.16.")
        || lower.starts_with("172.17.")
        || lower.starts_with("172.18.")
        || lower.starts_with("172.19.")
        || lower.starts_with("172.20.")
        || lower.starts_with("172.21.")
        || lower.starts_with("172.22.")
        || lower.starts_with("172.23.")
        || lower.starts_with("172.24.")
        || lower.starts_with("172.25.")
        || lower.starts_with("172.26.")
        || lower.starts_with("172.27.")
        || lower.starts_with("172.28.")
        || lower.starts_with("172.29.")
        || lower.starts_with("172.30.")
        || lower.starts_with("172.31.");
    if is_local {
        format!("http://{}", trimmed)
    } else {
        format!("https://{}", trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmission_client_is_object_safe() {
        fn _assert_dyn_compatible(_c: Arc<dyn DownloadClient>) {}
        let c = Arc::new(TransmissionClient::new(
            "http://localhost:9091",
            "transmission",
            "transmission",
            "ryokan",
        )) as Arc<dyn DownloadClient>;
        _assert_dyn_compatible(c);
    }

    #[test]
    fn sonarr_impl_name_is_transmission() {
        let c = TransmissionClient::new("http://localhost:9091", "", "", "ryokan");
        assert_eq!(c.sonarr_impl_name(), "Transmission");
    }

    #[test]
    fn empty_label_defaults_to_ryokan() {
        let c = TransmissionClient::new("http://localhost:9091", "", "", "");
        assert_eq!(c.label, "ryokan");
    }

    #[test]
    fn custom_label_preserved() {
        let c = TransmissionClient::new("http://localhost:9091", "", "", "anime-batch");
        assert_eq!(c.label, "anime-batch");
    }

    #[test]
    fn normalize_base_url_adds_scheme_for_local() {
        assert_eq!(
            normalize_base_url("localhost:9091"),
            "http://localhost:9091"
        );
        assert_eq!(
            normalize_base_url("192.168.1.5:9091"),
            "http://192.168.1.5:9091"
        );
    }

    #[test]
    fn normalize_base_url_preserves_explicit_scheme() {
        assert_eq!(
            normalize_base_url("https://seedbox.example.com/transmission"),
            "https://seedbox.example.com/transmission"
        );
    }

    #[test]
    fn normalize_base_url_trims_trailing_slash() {
        assert_eq!(
            normalize_base_url("http://localhost:9091/"),
            "http://localhost:9091"
        );
    }

    #[test]
    fn state_mapping_completion_semantics() {
        // Stopped + percentDone 1.0 → PausedComplete (complete).
        let stopped_complete = TxRawTorrent {
            status: 0,
            percent_done: 1.0,
            ..Default::default()
        };
        assert!(map_tx_state(&stopped_complete).is_complete());

        // Stopped + percentDone < 1.0 → Paused (not complete).
        let stopped_incomplete = TxRawTorrent {
            status: 0,
            percent_done: 0.5,
            ..Default::default()
        };
        assert!(!map_tx_state(&stopped_incomplete).is_complete());

        // Seeding (status 6) → Seeding (complete).
        let seeding = TxRawTorrent {
            status: 6,
            percent_done: 1.0,
            ..Default::default()
        };
        assert!(map_tx_state(&seeding).is_complete());

        // Downloading (status 4) → not complete even at 100% — the
        // status code is the authoritative signal for mid-transition
        // states; percentDone catches up on the next poll tick.
        let downloading = TxRawTorrent {
            status: 4,
            percent_done: 0.5,
            ..Default::default()
        };
        assert!(!map_tx_state(&downloading).is_complete());

        // Error string populated → Errored regardless of status.
        let errored = TxRawTorrent {
            status: 6,
            percent_done: 1.0,
            error_string: "tracker gone".into(),
            ..Default::default()
        };
        assert!(map_tx_state(&errored).is_errored());

        // Seeding + stalled → SeedingStalled, still complete.
        let seeding_stalled = TxRawTorrent {
            status: 6,
            percent_done: 1.0,
            is_stalled: true,
            ..Default::default()
        };
        assert_eq!(
            map_tx_state(&seeding_stalled),
            DownloadItemState::SeedingStalled
        );
        assert!(map_tx_state(&seeding_stalled).is_complete());
    }

    #[test]
    fn to_download_files_computes_progress_from_bytes() {
        let raw = TxRawTorrent {
            files: vec![
                TxRawFile {
                    name: "ep1.mkv".into(),
                    length: 1000,
                    bytes_completed: 500,
                },
                TxRawFile {
                    name: "ep2.mkv".into(),
                    length: 1000,
                    bytes_completed: 1000,
                },
            ],
            file_stats: vec![
                TxRawFileStat { wanted: true },
                TxRawFileStat { wanted: false },
            ],
            ..Default::default()
        };
        let files = to_download_files(&raw);
        assert_eq!(files.len(), 2);
        assert!((files[0].progress - 0.5).abs() < 1e-9);
        assert!((files[1].progress - 1.0).abs() < 1e-9);
        assert!(files[0].wanted);
        assert!(!files[1].wanted);
    }

    #[test]
    fn to_download_files_missing_file_stats_defaults_wanted_true() {
        // If fileStats drops short (shouldn't happen against real
        // Transmission, but defensive against malformed responses),
        // default to wanted=true rather than silently marking files as
        // skipped.
        let raw = TxRawTorrent {
            files: vec![TxRawFile {
                name: "ep1.mkv".into(),
                length: 100,
                bytes_completed: 0,
            }],
            file_stats: vec![],
            ..Default::default()
        };
        let files = to_download_files(&raw);
        assert_eq!(files.len(), 1);
        assert!(files[0].wanted);
    }

    #[test]
    fn label_filter_applied_on_listing() {
        // Smoke-test of the filter predicate used in list_scoped: only
        // torrents whose labels array contains the client's label are
        // kept.
        let label = "ryokan".to_string();
        let torrents = vec![
            TxRawTorrent {
                name: "scoped".into(),
                labels: vec!["ryokan".into()],
                ..Default::default()
            },
            TxRawTorrent {
                name: "other".into(),
                labels: vec!["other-tool".into()],
                ..Default::default()
            },
            TxRawTorrent {
                name: "nolabels".into(),
                labels: vec![],
                ..Default::default()
            },
        ];
        let kept: Vec<_> = torrents
            .into_iter()
            .filter(|t| t.labels.iter().any(|l| l == &label))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "scoped");
    }

    /// Live smoke test against a Transmission daemon at
    /// `http://localhost:9091` (lscr.io/linuxserver/transmission
    /// defaults — user `transmission`, password `transmission`).
    /// Opt in by running:
    ///
    ///     RYOKAN_TRANSMISSION_E2E=1 cargo test --release \
    ///       transmission::tests::live_smoke -- --ignored --nocapture
    ///
    /// Exercises the full surface Ryokan itself hits: test →
    /// add_torrent → list_scoped → duplicate-add → pause/resume →
    /// get_files → delete. Gated behind `#[ignore]` + env var so CI
    /// never depends on a daemon being up.
    #[tokio::test]
    #[ignore = "requires live Transmission at localhost:9091"]
    async fn live_smoke() {
        if std::env::var("RYOKAN_TRANSMISSION_E2E").is_err() {
            eprintln!(
                "skipping (set RYOKAN_TRANSMISSION_E2E=1 to run against localhost:9091)"
            );
            return;
        }

        let client = TransmissionClient::new(
            "http://localhost:9091",
            "transmission",
            "transmission",
            "ryokan-e2e",
        );

        let version = client.test().await.expect("test() failed");
        eprintln!("Transmission version: {version}");

        let magnet = "magnet:?xt=urn:btih:7a14d93f4c13e9c1ae255e0aa3b85a9aaf0cf52d&dn=sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce";
        let info_hash = "7a14d93f4c13e9c1ae255e0aa3b85a9aaf0cf52d";

        let outcome = client
            .add_torrent(magnet, info_hash)
            .await
            .expect("add_torrent() failed");
        eprintln!("add_torrent outcome: {outcome:?}");

        let list = client.list_scoped().await.expect("list_scoped() failed");
        eprintln!("scoped torrents: {}", list.len());
        let found = list
            .iter()
            .find(|t| t.hash.eq_ignore_ascii_case(info_hash))
            .expect("added torrent must appear in list_scoped");
        assert_eq!(
            found.category, "ryokan-e2e",
            "scoping label should round-trip as DownloadItem.category"
        );

        let dup = client
            .add_torrent(magnet, info_hash)
            .await
            .expect("duplicate add_torrent() failed");
        assert_eq!(dup, AddOutcome::AlreadyPresent);

        client.pause(info_hash).await.expect("pause() failed");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        client.resume(info_hash).await.expect("resume() failed");

        let _files = client
            .get_files(info_hash)
            .await
            .expect("get_files() failed");

        client
            .delete(info_hash, true)
            .await
            .expect("delete() failed");

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let after = client
            .list_scoped()
            .await
            .expect("list_scoped() post-delete failed");
        assert!(
            !after
                .iter()
                .any(|t| t.hash.eq_ignore_ascii_case(info_hash)),
            "torrent must not survive delete(_, delete_files=true)"
        );
        eprintln!("smoke passed");
    }
}
