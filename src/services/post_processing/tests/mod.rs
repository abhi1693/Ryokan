//! Post-processing tests, topic-split per the test-coverage-expansion
//! plan (PR 2). Each file covers one behavioral area of
//! `services::post_processing`:
//!
//!   * `file_ops.rs` — `do_file_op` across hardlink / copy / move
//!     modes, happy-path same-fs tests plus parent-directory
//!     creation, hardlink-produces-shared-inode property, move-mode
//!     source removal.
//!   * `filenames.rs` — `is_video_file` extension coverage and
//!     `sanitize_filename` behavior (pure-function helpers).
//!   * `lock.rs` — `POST_PROC_LOCK` serialization: the `try_lock` in
//!     `run_once` means a second run during an in-progress first
//!     returns early without stepping on the first's state.
//!
//! Cross-filesystem test paths (EXDEV hardlink fallback, cross-fs
//! move via `.ryokan-tmp`) are intentionally out of scope — they
//! require a second mounted filesystem to produce the errno, which
//! CI runners don't guarantee. The hardlink-on-fail path is still
//! covered by integration observation when the release binary runs
//! against a real download directory.

mod file_ops;
mod filenames;
mod lock;
