//! `do_file_op` coverage — the hardlink/copy/move orchestrator that
//! actually touches user media files. Each test runs against
//! `tempfile::TempDir` scratch space so no production data is
//! involved and the teardown is automatic. Every case here stays
//! within one filesystem (the tempdir) so the paths exercised are
//! the same-fs happy paths; cross-fs `EXDEV` / cross-fs move-tmp
//! paths require a second mounted FS and are skipped per the
//! module docstring.

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::services::post_processing::do_file_op;

fn write_src(dir: &TempDir, name: &str, body: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    let mut f = fs::File::create(&path).expect("create src");
    f.write_all(body).expect("write src body");
    path
}

// ─── Hardlink mode ─────────────────────────────────────────────────

#[tokio::test]
async fn hardlink_mode_lands_file_at_destination() {
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"payload");
    let dst = dir.path().join("subdir/dst.mkv");
    do_file_op("hardlink", &src, &dst).await.expect("hardlink");
    assert!(dst.exists(), "destination file should exist after hardlink");
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"payload",
        "destination content should match source"
    );
}

#[tokio::test]
async fn hardlink_mode_produces_shared_inode() {
    // A true hardlink means src and dst share the same inode number.
    // If the function silently copied instead, the inodes would differ
    // — which would mean every subsequent release-import doubles the
    // user's disk usage compared to the documented "hardlink seeds"
    // contract.
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"payload");
    let dst = dir.path().join("dst.mkv");
    do_file_op("hardlink", &src, &dst).await.unwrap();
    let src_ino = fs::metadata(&src).unwrap().ino();
    let dst_ino = fs::metadata(&dst).unwrap().ino();
    assert_eq!(
        src_ino, dst_ino,
        "hardlink mode should share inode; got {src_ino} vs {dst_ino}"
    );
}

#[tokio::test]
async fn hardlink_mode_leaves_source_intact() {
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"seeding-payload");
    let dst = dir.path().join("dst.mkv");
    do_file_op("hardlink", &src, &dst).await.unwrap();
    assert!(
        src.exists(),
        "hardlink mode must not remove source (torrent client still seeds it)"
    );
}

// ─── Copy mode ─────────────────────────────────────────────────────

#[tokio::test]
async fn copy_mode_lands_file_at_destination() {
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"copied-payload");
    let dst = dir.path().join("dst.mkv");
    do_file_op("copy", &src, &dst).await.unwrap();
    assert_eq!(fs::read(&dst).unwrap(), b"copied-payload");
}

#[tokio::test]
async fn copy_mode_produces_distinct_inodes() {
    // Opposite of the hardlink assertion: `copy` must produce a
    // physically separate file so a later edit to the source
    // (e.g., qBittorrent finalizing a partial download) doesn't
    // perturb what the library saw at import time.
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"payload");
    let dst = dir.path().join("dst.mkv");
    do_file_op("copy", &src, &dst).await.unwrap();
    let src_ino = fs::metadata(&src).unwrap().ino();
    let dst_ino = fs::metadata(&dst).unwrap().ino();
    assert_ne!(
        src_ino, dst_ino,
        "copy mode must produce a distinct inode; got {src_ino} for both"
    );
}

#[tokio::test]
async fn copy_mode_leaves_source_intact() {
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"seeding");
    let dst = dir.path().join("dst.mkv");
    do_file_op("copy", &src, &dst).await.unwrap();
    assert!(src.exists(), "copy must not remove source");
}

// ─── Move mode ─────────────────────────────────────────────────────

#[tokio::test]
async fn move_mode_lands_file_at_destination() {
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"moved-payload");
    let dst = dir.path().join("dst.mkv");
    do_file_op("move", &src, &dst).await.unwrap();
    assert_eq!(fs::read(&dst).unwrap(), b"moved-payload");
}

#[tokio::test]
async fn move_mode_removes_source_after_same_fs_rename() {
    // Same-fs `fs::rename` atomically relocates the file — the
    // source path must no longer exist after the op. Cross-fs move
    // (where rename fails with EXDEV and the fallback copies +
    // removes separately) is documented but not simulated here.
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"payload");
    let dst = dir.path().join("dst.mkv");
    do_file_op("move", &src, &dst).await.unwrap();
    assert!(
        !src.exists(),
        "move must remove source after successful rename"
    );
    assert!(dst.exists(), "destination must exist after move");
}

#[tokio::test]
async fn move_mode_preserves_content_byte_for_byte() {
    // Non-trivial payload so a silent truncation bug would show up.
    let dir = TempDir::new().unwrap();
    let payload: Vec<u8> = (0..=255).cycle().take(16_384).collect();
    let src = write_src(&dir, "src.mkv", &payload);
    let dst = dir.path().join("dst.mkv");
    do_file_op("move", &src, &dst).await.unwrap();
    assert_eq!(fs::read(&dst).unwrap(), payload);
}

// ─── Parent-directory creation ────────────────────────────────────

#[tokio::test]
async fn hardlink_mode_creates_missing_parent_directories() {
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"x");
    // Destination nested under two unseen directories.
    let dst = dir.path().join("a/b/c/dst.mkv");
    assert!(
        !dst.parent().unwrap().exists(),
        "precondition: parent must not exist yet"
    );
    do_file_op("hardlink", &src, &dst).await.unwrap();
    assert!(dst.exists());
    assert!(dst.parent().unwrap().is_dir());
}

#[tokio::test]
async fn copy_mode_creates_missing_parent_directories() {
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"x");
    let dst = dir.path().join("a/b/dst.mkv");
    do_file_op("copy", &src, &dst).await.unwrap();
    assert!(dst.exists());
}

#[tokio::test]
async fn move_mode_creates_missing_parent_directories() {
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"x");
    let dst = dir.path().join("a/b/dst.mkv");
    do_file_op("move", &src, &dst).await.unwrap();
    assert!(dst.exists());
    assert!(!src.exists());
}

// ─── Unknown mode falls through to hardlink default ────────────────

#[tokio::test]
async fn unknown_mode_string_falls_back_to_hardlink_default() {
    // `do_file_op`'s `match mode` dispatches "move" / "copy" and
    // everything else falls through to hardlink. Settings-save
    // validation constrains the enum on the write side; on the read
    // side, an unexpected string (migrating from older config, manual
    // DB edit) should not panic or drop data — it should import.
    let dir = TempDir::new().unwrap();
    let src = write_src(&dir, "src.mkv", b"unknown-mode-payload");
    let dst = dir.path().join("dst.mkv");
    do_file_op("garbage-never-seen-before", &src, &dst)
        .await
        .unwrap();
    assert_eq!(fs::read(&dst).unwrap(), b"unknown-mode-payload");
    // Source still exists — fallback is hardlink, not move.
    assert!(src.exists());
    assert_eq!(
        fs::metadata(&src).unwrap().ino(),
        fs::metadata(&dst).unwrap().ino(),
        "hardlink default should share inode"
    );
}

// ─── Missing source surfaces the error ─────────────────────────────

#[tokio::test]
async fn missing_source_returns_error_not_panic() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("does-not-exist.mkv");
    let dst = dir.path().join("dst.mkv");
    let err = do_file_op("copy", &src, &dst).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(!dst.exists());
}
