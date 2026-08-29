use crate::services::download_client::DownloadFile;
use crate::services::post_processing::{
    effective_batch_shape, is_secondary_video_path, ready_wanted_video_indices,
    requires_episode_map_preflight, validate_batch_episode_map,
};

#[test]
fn accepts_unique_dot_delimited_complete_series_batch() {
    let files = vec![
        (
            0,
            42,
            None,
            0,
            "[SoM] Dragon.Ball.001.V2.DVD.480p.mkv".to_string(),
        ),
        (
            1,
            42,
            None,
            0,
            "[SoM] Dragon.Ball.002.V2.DVD.480p.mkv".to_string(),
        ),
        (
            2,
            42,
            None,
            0,
            "[SoM] Dragon.Ball.153.DVD.480p.mkv".to_string(),
        ),
    ];
    assert!(validate_batch_episode_map(&files).is_ok());
}

#[test]
fn unparseable_extras_are_skipped_without_failing_the_batch() {
    // Real packs routinely ship NCOP/NCED/PV/CM/menu videos that parse to
    // `None` by design. They must be absent from the plan (the import loop
    // warn-skips them) without stranding the parseable siblings.
    let files = vec![
        (0, 42, None, 0, "Dragon.Ball.001.mkv".to_string()),
        (1, 42, None, 0, "Dragon.Ball.movie.mkv".to_string()),
        (
            2,
            42,
            None,
            0,
            "[Moozzi2] Anne of Green Gables [SP01] NCOP (BD 1440x1080 x.265 Flac).mkv".to_string(),
        ),
        (3, 42, None, 0, "Dragon.Ball.002.mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    assert!(plan.contains_key(&0));
    assert!(plan.contains_key(&3));
    assert!(!plan.contains_key(&1));
    assert!(!plan.contains_key(&2));
}

#[test]
fn non_positive_resolved_episode_is_skipped_without_failing_the_batch() {
    // E00 specials (and files landing on a sibling whose offset exceeds
    // their own number) resolve non-positive; skip them, keep the rest.
    let files = vec![
        (0, 42, None, 0, "Show - 00 - Special.mkv".to_string()),
        (1, 42, None, 0, "Show - 01.mkv".to_string()),
    ];
    let plan = validate_batch_episode_map(&files).unwrap();
    assert!(!plan.contains_key(&0));
    assert!(plan.contains_key(&1));
}

#[test]
fn rejects_duplicate_destination_within_batch() {
    let files = vec![
        (0, 42, None, 0, "Show.S01E01.first.mkv".to_string()),
        (1, 42, None, 0, "Show.S01E01.second.mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("mapped both"));
    assert!(err.contains("series 42 episode 1"));
    assert!(err.contains("no files were changed"));
}

#[test]
fn misclassified_multi_video_grab_still_rejects_duplicate_destination() {
    let is_batch = false;
    let files = vec![
        (0, 42, None, 0, "Show.S01E01.first.mkv".to_string()),
        (1, 42, None, 0, "Show.S01E01.second.mkv".to_string()),
    ];

    assert!(!is_batch);
    assert!(requires_episode_map_preflight(is_batch, files.len()));
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("mapped both"));
    assert!(err.contains("series 42 episode 1"));
}

#[test]
fn multi_video_file_shape_corrects_a_misclassified_single_grab() {
    assert!(effective_batch_shape(false, 220));
    assert!(effective_batch_shape(true, 1));
    assert!(!effective_batch_shape(false, 1));
}

#[test]
fn same_episode_is_valid_when_routed_to_different_series() {
    let files = vec![
        (0, 42, None, 0, "Parent.S01E01.mkv".to_string()),
        (1, 43, None, 0, "Sibling.S01E01.mkv".to_string()),
    ];
    assert!(validate_batch_episode_map(&files).is_ok());
}

#[test]
fn route_offsets_are_checked_against_final_episode_slot() {
    let files = vec![
        (0, 43, Some(12), 0, "Arc.S01E13.first.mkv".to_string()),
        (1, 43, Some(13), 0, "Arc.S01E14.second.mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("series 43 episode 1"));
}

#[test]
fn cumulative_offset_collision_is_rejected_before_import() {
    let files = vec![
        (0, 42, None, 12, "Show.S01E01.mkv".to_string()),
        (1, 42, None, 12, "Show.013.mkv".to_string()),
    ];
    let err = validate_batch_episode_map(&files).unwrap_err();
    assert!(err.contains("series 42 episode 1"));
}

#[test]
fn incomplete_wanted_video_blocks_the_entire_batch() {
    let files = vec![
        DownloadFile {
            name: "Show.001.mkv".to_string(),
            size: 100,
            progress: 1.0,
            wanted: true,
        },
        DownloadFile {
            name: "Show.002.mkv".to_string(),
            size: 100,
            progress: 0.5,
            wanted: true,
        },
    ];
    let err = ready_wanted_video_indices(&files).unwrap_err();
    assert!(err.contains("1 of 2 wanted video files are incomplete"));
}

#[test]
fn complete_unwanted_video_is_excluded_from_import_plan() {
    let files = vec![
        DownloadFile {
            name: "Show.001.mkv".to_string(),
            size: 100,
            progress: 1.0,
            wanted: true,
        },
        DownloadFile {
            name: "Show.002.mkv".to_string(),
            size: 100,
            progress: 1.0,
            wanted: false,
        },
    ];
    assert_eq!(ready_wanted_video_indices(&files).unwrap(), vec![0]);
}

#[test]
fn nested_naruto_extras_are_not_episode_import_candidates() {
    let root = "Naruto.v4.480p.DVD.Dual-Audio.FLAC2.0.Hi10P.x264-JySzE";
    assert!(is_secondary_video_path(&format!(
        "{root}/Extras/Canon Cut/Naruto.135-220.Canon.Cut.mkv"
    )));
    assert!(is_secondary_video_path(&format!(
        "{root}/Extras/NCED/NCED.01.mkv"
    )));
    assert!(!is_secondary_video_path(&format!(
        "{root}/Naruto.001.v4.480p.DVD.mkv"
    )));
}

#[test]
fn extras_label_in_release_root_does_not_hide_real_episodes() {
    let root = "[JySzE] Naruto Shippuden [Complete] [Extras] [x264]";
    assert!(!is_secondary_video_path(&format!(
        "{root}/Naruto.Shippuden.001.mkv"
    )));
    assert!(is_secondary_video_path(&format!(
        "{root}/Extras/Creditless Openings/Opening 01.mkv"
    )));
    assert!(is_secondary_video_path("Extras/NCOP.01.mkv"));
}
