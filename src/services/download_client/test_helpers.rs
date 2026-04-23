//! Shared helpers for the per-client `live_smoke*` tests. Only
//! compiled under `#[cfg(test)]`.
//!
//! The smoke tests exercise each `DownloadClient` impl against a
//! real running daemon (qBittorrent / Deluge / Transmission /
//! rtorrent on localhost). They use synthetic `.torrent` files
//! built on-the-fly via shell-out to `transmission-create` rather
//! than depending on a public magnet's DHT metadata fetch landing
//! during the test window — magnets are flaky in containerized
//! networks and make the tests non-deterministic. The synthetic
//! approach gives every test run the same file layout with the same
//! priorities-post-narrow assertions.

use std::path::PathBuf;

/// Build a synthetic multi-file `.torrent` in a tempdir via
/// `transmission-create`. Returns `(tempdir_guard, torrent_path)` —
/// the tempdir handle must outlive the test so the dummy files
/// remain on disk for clients that check file-existence at add time
/// (some clients refuse to add a torrent whose source files are
/// missing). Returns `None` with a printed skip message if
/// `transmission-create` isn't installed.
///
/// File layout (7 files, mirrors a typical anime batch):
/// * `testpack/episode_1.mkv` .. `episode_5.mkv` — 8 KB each, the
///   "wanted" subset for narrowing tests.
/// * `testpack/sample.mkv` — 2 KB, what `pick_wanted_file_indices`
///   classifies as likely-unwanted extras.
/// * `testpack/readme.txt` — 512 B, explicitly unwanted sidecar.
///
/// Content is all-zero bytes. That's fine because the smoke tests
/// always pause immediately after upload and verify priorities
/// without ever attempting to actually download content (the fake
/// tracker URL is unreachable anyway).
pub(crate) fn build_testpack_torrent() -> Option<(tempfile::TempDir, PathBuf)> {
    if std::process::Command::new("transmission-create")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: transmission-create not installed");
        return None;
    }
    let tmp = tempfile::tempdir().expect("tempdir creation failed");
    let pack_dir = tmp.path().join("testpack");
    std::fs::create_dir(&pack_dir).expect("create testpack dir");
    for i in 1..=5 {
        std::fs::write(pack_dir.join(format!("episode_{i}.mkv")), vec![0u8; 8192])
            .expect("write episode file");
    }
    std::fs::write(pack_dir.join("sample.mkv"), vec![0u8; 2048]).expect("write sample");
    std::fs::write(pack_dir.join("readme.txt"), b"test readme").expect("write readme");
    let torrent_path = tmp.path().join("testpack.torrent");
    let output = std::process::Command::new("transmission-create")
        .args([
            "-o",
            torrent_path.to_str().unwrap(),
            "-t",
            "http://localhost:1/announce",
        ])
        .arg(&pack_dir)
        .output()
        .expect("transmission-create spawn failed");
    assert!(
        output.status.success(),
        "transmission-create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some((tmp, torrent_path))
}
