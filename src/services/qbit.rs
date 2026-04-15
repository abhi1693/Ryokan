use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

/// qBittorrent Web API client with automatic re-authentication.
#[derive(Clone)]
pub struct QbitClient {
    base_url: String,
    user: String,
    pass: String,
    category: String,
    http: Client,
    logged_in: Arc<Mutex<bool>>,
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct Torrent {
    pub hash: String,
    pub name: String,
    pub size: i64,
    pub progress: f64,
    pub dlspeed: i64,
    pub state: String,
    pub category: String,
    pub eta: i64,
    #[serde(default)]
    pub save_path: String,
}

#[derive(Debug, Deserialize)]
pub struct TorrentFile {
    /// Relative path of the file within the torrent (from save_path).
    pub name: String,
    pub size: i64,
    pub progress: f64,
    /// qBit file priority: 0 = skip, 1 = normal, 6 = high, 7 = max.
    /// Defaulted to `1` (normal) on the off chance qBit omits the
    /// field — safer than defaulting to 0 "skip", which our
    /// additive-merge logic interprets as "this torrent has been
    /// narrowed before".
    #[serde(default = "default_file_priority")]
    pub priority: i32,
}

fn default_file_priority() -> i32 {
    1
}

/// Outcome of an `add_torrent_with_file_filter` call.
#[derive(Debug)]
pub enum SelectiveOutcome {
    /// Filter narrowed the torrent to specific files. Contains the
    /// kept file indices (always a strict subset of the file list).
    Filtered(Vec<usize>),
    /// No filter applied — the torrent is downloading all files.
    /// Used when the caller's `pick` returned `None`, when the pick
    /// matched every file (not a megapack after all), or when qBit
    /// metadata fetch timed out and we resumed the already-added
    /// torrent unchanged instead of leaving it stuck paused.
    FullDownload,
}

impl QbitClient {
    pub fn new(base_url: &str, user: &str, pass: &str, category: &str) -> Self {
        let http = Client::builder()
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: normalize_base_url(base_url),
            user: user.to_string(),
            pass: pass.to_string(),
            category: category.to_string(),
            http,
            logged_in: Arc::new(Mutex::new(false)),
        }
    }

    /// Authenticate with qBittorrent.
    pub async fn login(&self) -> Result<(), String> {
        let resp = self
            .http
            .post(format!("{}/api/v2/auth/login", self.base_url))
            .form(&[("username", &self.user), ("password", &self.pass)])
            .send()
            .await
            .map_err(|e| format!("qbit login failed: {}", e))?;

        let body = resp.text().await.unwrap_or_default();
        if body == "Fails." {
            return Err("qbit auth failed: invalid credentials".into());
        }

        *self.logged_in.lock().await = true;
        Ok(())
    }

    /// Ensure we're logged in.
    async fn ensure_login(&self) -> Result<(), String> {
        let logged_in = *self.logged_in.lock().await;
        if !logged_in {
            self.login().await?;
        }
        Ok(())
    }

    /// Perform a GET with automatic re-auth on 403.
    async fn do_get(&self, endpoint: &str) -> Result<reqwest::Response, String> {
        self.ensure_login().await?;

        let url = format!("{}{}", self.base_url, endpoint);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if resp.status() == StatusCode::FORBIDDEN {
            *self.logged_in.lock().await = false;
            self.login().await?;
            self.http
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("retry failed: {}", e))
        } else {
            Ok(resp)
        }
    }

    /// Perform a POST with form data and automatic re-auth on 403.
    async fn do_post_form(&self, endpoint: &str, form: &[(&str, &str)]) -> Result<reqwest::Response, String> {
        self.ensure_login().await?;

        let url = format!("{}{}", self.base_url, endpoint);
        let resp = self
            .http
            .post(&url)
            .form(form)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if resp.status() == StatusCode::FORBIDDEN {
            *self.logged_in.lock().await = false;
            self.login().await?;
            self.http
                .post(&url)
                .form(form)
                .send()
                .await
                .map_err(|e| format!("retry failed: {}", e))
        } else {
            Ok(resp)
        }
    }

    /// Add a magnet or torrent URL to qBittorrent.
    pub async fn add_torrent(&self, url: &str) -> Result<(), String> {
        let form = [("urls", url), ("category", &self.category)];
        let resp = self.do_post_form("/api/v2/torrents/add", &form).await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("qbit add failed: {}", body));
        }
        Ok(())
    }

    /// Set the download priority for a list of file indices inside a
    /// single torrent. `priority` meanings:
    ///   0 = do not download (skip)
    ///   1 = normal
    ///   6 = high
    ///   7 = maximum
    ///
    /// qBit expects the file ids joined with `|`.
    pub async fn set_file_priority(
        &self,
        hash: &str,
        file_ids: &[usize],
        priority: i32,
    ) -> Result<(), String> {
        if file_ids.is_empty() {
            return Ok(());
        }
        let id_str = file_ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("|");
        let prio_str = priority.to_string();
        let form = [
            ("hash", hash),
            ("id", id_str.as_str()),
            ("priority", prio_str.as_str()),
        ];
        let resp = self.do_post_form("/api/v2/torrents/filePrio", &form).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("qbit filePrio failed: {} {}", status, body.trim()));
        }
        Ok(())
    }

    /// Poll the torrent's file list until qBit has fetched metadata and
    /// knows the contents, or `timeout` elapses. For magnet links added
    /// paused, the file list is not immediately available — qBit has to
    /// hit trackers and pull the metadata first. Returns the final file
    /// list on success.
    pub async fn wait_for_metadata(
        &self,
        hash: &str,
        timeout: std::time::Duration,
    ) -> Result<Vec<TorrentFile>, String> {
        let start = std::time::Instant::now();
        let mut delay = std::time::Duration::from_millis(500);
        loop {
            match self.get_torrent_files(hash).await {
                Ok(files) if !files.is_empty() => return Ok(files),
                Ok(_) => {}
                Err(e) => {
                    // 404 while qBit is still discovering the torrent —
                    // treat as "not ready yet" until the timeout expires.
                    if start.elapsed() >= timeout {
                        return Err(format!(
                            "qbit metadata fetch error after {:?}: {}",
                            timeout, e
                        ));
                    }
                }
            }
            if start.elapsed() >= timeout {
                return Err(format!(
                    "qbit metadata fetch timed out after {:?}",
                    timeout
                ));
            }
            tokio::time::sleep(delay).await;
            // Gentle backoff, capped at 2s per poll.
            delay = (delay * 2).min(std::time::Duration::from_secs(2));
        }
    }

    /// Add a torrent, wait for qBit to know its file list, invoke `pick`
    /// to select which files to keep, and mark the rest as skip. On
    /// metadata-fetch timeout, or when `pick` returns `None` / an empty
    /// list / a set that covers every file, the torrent is left
    /// downloading unfiltered so the user still gets a full grab.
    ///
    /// Notably: we do **not** add paused. qBit 5.x renamed pause/resume
    /// to stop/start and — more importantly — a torrent added in
    /// stopped state doesn't publish its file list through
    /// `/torrents/files`, so the old "add paused → wait metadata → set
    /// priorities → resume" flow hangs forever on 5.x. Instead we add
    /// unpaused and race `filePrio` against qBit's peer-discovery
    /// startup. For a `.torrent` URL add, qBit parses the file list
    /// within a couple of seconds — well before any real data transfer
    /// begins — so the window where unwanted pieces might be requested
    /// is small and bounded. An explicit `resume_torrent` call after
    /// the add handles the dedup case where qBit already has the same
    /// info hash sitting in stopped state from an earlier failed grab.
    ///
    /// **Additive merge**: when a second selective grab lands on a
    /// torrent that's already been narrowed by an earlier grab (e.g.
    /// the user grabbed Kizu 2 from a Monogatari megapack, then grabs
    /// Kizu 1 from the same pack), we only bump the *new* wanted
    /// files from skip → normal. We do **not** re-skip everything
    /// outside the new keep set, because that would silently clobber
    /// files that earlier grabs deliberately enabled. Detection is by
    /// looking at current file priorities in the qBit file list: if
    /// any file is currently at priority 0, the torrent has been
    /// narrowed before and we take the merge branch.
    pub async fn add_torrent_with_file_filter<F>(
        &self,
        url: &str,
        info_hash: &str,
        pick: F,
    ) -> Result<SelectiveOutcome, String>
    where
        F: FnOnce(&[String]) -> Option<Vec<usize>>,
    {
        if info_hash.is_empty() {
            return Err("selective download requires a known info hash".into());
        }
        let hash_lc = info_hash.to_ascii_lowercase();

        self.add_torrent(url).await?;

        // If qBit already had this hash sitting in stopped state from
        // a prior failed attempt, the add above is a dedupe no-op and
        // the torrent is still stopped. Explicitly start it so
        // metadata starts flowing and the file list becomes visible.
        // Fresh adds are already running, so this is a no-op for them.
        self.resume_torrent(&hash_lc).await?;

        // With a `.torrent` URL qBit has the file list within 1-3
        // seconds; 10s is a generous ceiling that keeps the HTTP
        // handler responsive. On timeout we give up on narrowing and
        // let the full download proceed — the torrent is running, not
        // stuck. A longer wait here would block the Grab button in
        // the UI for the full timeout, which is a worse UX than just
        // downloading the whole pack when metadata is slow.
        let files = match self
            .wait_for_metadata(&hash_lc, std::time::Duration::from_secs(10))
            .await
        {
            Ok(files) => files,
            Err(_) => return Ok(SelectiveOutcome::FullDownload),
        };

        let names: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
        let keep = pick(&names);

        let new_keep_ids = match keep {
            Some(ids) if !ids.is_empty() && ids.len() < files.len() => ids,
            _ => return Ok(SelectiveOutcome::FullDownload),
        };

        // Has an earlier grab already narrowed this torrent? Any file
        // currently at priority 0 proves yes — qBit only sets 0 when
        // something explicitly skipped it, not as a default.
        let already_narrowed = files.iter().any(|f| f.priority == 0);

        if already_narrowed {
            // Merge branch: only bump new keeps that are currently at
            // skip. Files already > 0 stay untouched so previous
            // grabs on this megapack keep downloading.
            let to_bump: Vec<usize> = new_keep_ids
                .iter()
                .copied()
                .filter(|&i| files.get(i).map(|f| f.priority == 0).unwrap_or(false))
                .collect();
            if !to_bump.is_empty() {
                self.set_file_priority(&hash_lc, &to_bump, 1).await?;
            }
        } else {
            // Fresh branch: apply the full skip/keep split.
            let skip_ids: Vec<usize> = (0..files.len())
                .filter(|i| !new_keep_ids.contains(i))
                .collect();
            self.set_file_priority(&hash_lc, &skip_ids, 0).await?;
            self.set_file_priority(&hash_lc, &new_keep_ids, 1).await?;
        }
        Ok(SelectiveOutcome::Filtered(new_keep_ids))
    }

    /// Get all torrents, optionally filtered by category.
    pub async fn get_torrents(&self) -> Result<Vec<Torrent>, String> {
        let endpoint = if self.category.is_empty() {
            "/api/v2/torrents/info".to_string()
        } else {
            format!("/api/v2/torrents/info?category={}", self.category)
        };

        let resp = self.do_get(&endpoint).await?;
        let torrents: Vec<Torrent> = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse torrents: {}", e))?;

        Ok(torrents)
    }

    /// Get the files inside a specific torrent.
    ///
    /// qBit 5.x returns **HTTP 404 with a plain-text `"Not Found"` body**
    /// for `/torrents/files?hash=X` while the torrent is still fetching
    /// metadata — not an empty JSON array. `reqwest::Response::json()`
    /// does not look at the status code, so calling it on a 404 body
    /// hits the serde parser against `"Not Found"` and fails with
    /// `"error decoding response body"`. We were then treating that as
    /// a real error and burning the full 60s timeout in
    /// [`wait_for_metadata`]'s retry loop even though the torrent was
    /// just a couple seconds away from being ready.
    ///
    /// Returning `Ok(vec![])` on 404 lets the wait loop's "empty list →
    /// retry" arm drive the poll, the same way it would for a
    /// pre-5.x qBit that returns `[]` in the not-ready state.
    pub async fn get_torrent_files(&self, hash: &str) -> Result<Vec<TorrentFile>, String> {
        let endpoint = format!("/api/v2/torrents/files?hash={}", hash);
        let resp = self.do_get(&endpoint).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("qbit torrent files fetch failed: {} {}", status, body.trim()));
        }
        let files: Vec<TorrentFile> = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse torrent files: {}", e))?;
        Ok(files)
    }

    /// Pause a torrent by hash. qBit 5.x renamed `/torrents/pause` to
    /// `/torrents/stop`; we try the new name first and fall back to the
    /// old one so both generations work without a version probe.
    pub async fn pause_torrent(&self, hash: &str) -> Result<(), String> {
        let form = [("hashes", hash)];
        let resp = self.do_post_form("/api/v2/torrents/stop", &form).await?;
        if resp.status().is_success() {
            return Ok(());
        }
        // Fallback for qBit ≤ 4.x.
        let resp = self.do_post_form("/api/v2/torrents/pause", &form).await?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("qbit pause failed: {} {}", status, body.trim()))
    }

    /// Resume a torrent by hash. qBit 5.x renamed `/torrents/resume` to
    /// `/torrents/start`; we try the new name first and fall back to
    /// the old one so the 4.x → 5.x transition is transparent.
    pub async fn resume_torrent(&self, hash: &str) -> Result<(), String> {
        let form = [("hashes", hash)];
        let resp = self.do_post_form("/api/v2/torrents/start", &form).await?;
        if resp.status().is_success() {
            return Ok(());
        }
        // Fallback for qBit ≤ 4.x.
        let resp = self.do_post_form("/api/v2/torrents/resume", &form).await?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("qbit resume failed: {} {}", status, body.trim()))
    }

    /// Delete a torrent by hash (optionally delete files).
    pub async fn delete_torrent(&self, hash: &str, delete_files: bool) -> Result<(), String> {
        let delete_str = if delete_files { "true" } else { "false" };
        let form = [("hashes", hash), ("deleteFiles", delete_str)];
        let resp = self.do_post_form("/api/v2/torrents/delete", &form).await?;
        if !resp.status().is_success() {
            return Err("Failed to delete torrent".into());
        }
        Ok(())
    }

    /// Test the connection by fetching the app version.
    pub async fn test_connection(&self) -> Result<String, String> {
        let resp = self.do_get("/api/v2/app/version").await?;
        let version = resp.text().await.unwrap_or_default();
        Ok(version)
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

    let lower = trimmed.to_ascii_lowercase();
    let is_local = lower.starts_with("localhost")
        || lower.starts_with("127.")
        || lower.starts_with("10.")
        || lower.starts_with("192.168.")
        || lower.starts_with("172.16.")
        || lower.starts_with("172.17.")
        || lower.starts_with("172.18.")
        || lower.starts_with("172.19.")
        || lower.starts_with("172.2")
        || lower.starts_with("172.30.")
        || lower.starts_with("172.31.");

    if is_local {
        format!("http://{}", trimmed)
    } else {
        format!("https://{}", trimmed)
    }
}
