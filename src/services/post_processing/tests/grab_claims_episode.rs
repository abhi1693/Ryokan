//! `grab_claims_episode` coverage — the import-loop guard that keeps
//! a single-episode grab from sweeping in stranger episodes when
//! `walk_video_files` is given the parent complete dir (the SAB
//! `storage`-points-at-parent edge case). Pinning the four cases:
//!
//!   1. Strict: `episode_numbers=[N]` claims only N.
//!   2. Permissive on batch (`is_batch=true`).
//!   3. Permissive on route coverage (Phase-2 sibling-routed imports).
//!   4. Permissive on empty `episode_numbers` (legacy back-compat).
//!
//! Case 4 is the one most likely to drift: a future "tighten the guard"
//! refactor could silently start dropping imports for legacy grabs
//! whose episode list never got populated. The explicit test here is
//! the contract.

use crate::services::post_processing::grab_claims_episode;

#[test]
fn strict_match_when_grab_has_explicit_episode_list() {
    // Single-episode grab claiming ep 1156. A walk that finds 1156 →
    // claimed; one that finds 1154 (a stranger from a sibling job) →
    // dropped.
    assert!(
        grab_claims_episode(false, false, &[1156], 1156),
        "grab claims its own episode"
    );
    assert!(
        !grab_claims_episode(false, false, &[1156], 1154),
        "grab does NOT claim a stranger episode"
    );
}

#[test]
fn batch_grab_claims_anything() {
    // is_batch=1 is set on grabs that legitimately span many episodes
    // (season packs, etc.). Their episode_numbers may be empty or
    // partial; the guard must permit every walked file regardless.
    assert!(grab_claims_episode(true, false, &[], 1));
    assert!(grab_claims_episode(true, false, &[], 99));
    // Even when episode_numbers IS populated, batch wins.
    assert!(grab_claims_episode(true, false, &[1, 2, 3], 999));
}

#[test]
fn route_covered_file_claims_through_route() {
    // Phase-2 sibling-routed imports (the smol Monogatari /
    // JoJo-Egypt-hen shape) are non-batch but get a route row per
    // file. The route's per-sibling `episode_numbers` lives outside
    // this guard; route presence alone is sufficient.
    assert!(grab_claims_episode(false, true, &[1156], 9999));
}

#[test]
fn empty_episode_numbers_permits_everything_legacy_back_compat() {
    // Legacy grabs from before `episode_numbers` was reliably
    // populated can have an empty array. Permissive-on-empty
    // preserves backward compatibility — tightening this would
    // silently break imports for those rows. Pin it.
    assert!(grab_claims_episode(false, false, &[], 1));
    assert!(grab_claims_episode(false, false, &[], 1156));
    assert!(grab_claims_episode(false, false, &[], 99999));
}

#[test]
fn precedence_holds_when_multiple_permissive_signals_apply() {
    // Belt-and-suspenders: a row that's both batch AND route-covered
    // AND has an empty list AND happens to match the parsed episode
    // is still claimed. No interaction between the OR clauses.
    assert!(grab_claims_episode(true, true, &[], 1));
    assert!(grab_claims_episode(true, true, &[1], 1));
}
