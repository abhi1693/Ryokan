//! Phase 1 Deluge stub.
//!
//! Exists to exercise the [`DownloadClient`] trait shape against a
//! second implementation at compile time, so any qBit-specific
//! assumption that accidentally leaked into the trait surfaces as a
//! type error *before* the 15-handler refactor in Phase 1 commits —
//! not in Phase 2 when changing the trait means re-touching every
//! call site. Phase 2 replaces this file with the real Deluge Web
//! UI JSON-RPC client (see ~/Documents/ryokan-plan-pluggable-
//! download-clients.md → Phase 2).
//!
//! Not constructed, not wired into `AppState`. All mutating methods
//! are `unimplemented!()`; only `test()` and `sonarr_impl_name()`
//! return stub values so downstream code can reference the type
//! if it ever does.

use async_trait::async_trait;

use super::{AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome};

#[allow(dead_code)]
pub struct DelugeClient {
    base_url: String,
    password: String,
    label: String,
}

#[allow(dead_code)]
impl DelugeClient {
    pub fn new(base_url: &str, password: &str, label: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            password: password.to_string(),
            label: label.to_string(),
        }
    }
}

#[async_trait]
impl DownloadClient for DelugeClient {
    async fn test(&self) -> Result<String, String> {
        Err("Deluge client not implemented — Phase 2".into())
    }

    async fn add_torrent(&self, _url: &str, _info_hash: &str) -> Result<AddOutcome, String> {
        unimplemented!("Deluge impl lands in Phase 2")
    }

    async fn add_torrent_with_file_filter(
        &self,
        _url: &str,
        _info_hash: &str,
        _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        unimplemented!("Deluge impl lands in Phase 2")
    }

    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        unimplemented!("Deluge impl lands in Phase 2")
    }

    async fn get_files(&self, _info_hash: &str) -> Result<Vec<DownloadFile>, String> {
        unimplemented!("Deluge impl lands in Phase 2")
    }

    async fn pause(&self, _info_hash: &str) -> Result<(), String> {
        unimplemented!("Deluge impl lands in Phase 2")
    }

    async fn resume(&self, _info_hash: &str) -> Result<(), String> {
        unimplemented!("Deluge impl lands in Phase 2")
    }

    async fn delete(&self, _info_hash: &str, _delete_files: bool) -> Result<(), String> {
        unimplemented!("Deluge impl lands in Phase 2")
    }

    async fn set_file_wanted(
        &self,
        _info_hash: &str,
        _files: &[usize],
        _wanted: bool,
    ) -> Result<(), String> {
        unimplemented!("Deluge impl lands in Phase 2")
    }

    fn sonarr_impl_name(&self) -> &'static str {
        "Deluge"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing test: compiles iff the trait is object-safe
    /// and the Deluge stub's signatures match the trait. If either
    /// breaks, this file fails to compile — which is the whole point
    /// of having the stub during Phase 1.
    #[test]
    fn deluge_stub_is_object_safe() {
        fn _assert_dyn_compatible(_c: std::sync::Arc<dyn DownloadClient>) {}
        let deluge = std::sync::Arc::new(DelugeClient::new("http://x:8112", "", "ryokan"))
            as std::sync::Arc<dyn DownloadClient>;
        _assert_dyn_compatible(deluge);
    }

    #[tokio::test]
    async fn deluge_stub_sonarr_impl_name() {
        let d = DelugeClient::new("http://x:8112", "", "ryokan");
        assert_eq!(d.sonarr_impl_name(), "Deluge");
    }
}
