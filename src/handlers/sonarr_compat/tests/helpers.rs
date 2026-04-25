//! Pure-helper coverage for the Sonarr shim — `ratings_from_score`
//! and `map_status` are tiny but load-bearing. Both feed every series
//! payload Seerr sees. A wrong rating denominator (Sonarr expects
//! 0-10, AL is 0-100) flashes an obviously-broken score in the
//! Seerr UI; a wrong `map_status` value silently corrupts Seerr's
//! "show ended" detection so finished anime never get marked as
//! such on its dashboard.

use crate::handlers::sonarr_compat::helpers::{map_status, ratings_from_score};

// ── ratings_from_score ───────────────────────────────────────────────

#[test]
fn ratings_from_score_none_renders_zeroed() {
    // The "metadata never refreshed" shape — Sonarr itself emits
    // this for new entries, so Seerr handles it cleanly.
    let r = ratings_from_score(None);
    assert_eq!(r.votes, 0);
    assert_eq!(r.value, 0.0);
}

#[test]
fn ratings_from_score_zero_or_negative_renders_zeroed() {
    // AL emits `averageScore: null` as Some(0) after our parser's
    // `unwrap_or(0)` fallback. Treat both 0 and the (theoretically
    // impossible) negative case as "no rating" rather than letting
    // a 0.0 value rendered in Seerr suggest a real bottom-of-scale
    // score.
    assert_eq!(ratings_from_score(Some(0)).value, 0.0);
    assert_eq!(ratings_from_score(Some(-5)).value, 0.0);
}

#[test]
fn ratings_from_score_divides_by_ten_for_zero_to_ten_scale() {
    // AL: 0-100 integer. Sonarr/Seerr: 0-10 float. Pin the
    // denominator at 10 — a regression that drops the divisor would
    // produce visibly broken 8500-style ratings in Seerr.
    assert_eq!(ratings_from_score(Some(85)).value, 8.5);
    assert_eq!(ratings_from_score(Some(100)).value, 10.0);
    assert_eq!(ratings_from_score(Some(1)).value, 0.1);
}

#[test]
fn ratings_from_score_votes_always_zero() {
    // We don't have a vote count from AL/MAL that maps cleanly to
    // Sonarr's notion. 0-with-a-non-zero-value is the shape Sonarr
    // itself emits for unrated newer entries, so Seerr handles it.
    for s in [None, Some(0), Some(50), Some(100)] {
        assert_eq!(ratings_from_score(s).votes, 0, "score {:?}", s);
    }
}

// ── map_status ───────────────────────────────────────────────────────

#[test]
fn map_status_releasing_to_continuing() {
    // AL's "RELEASING" + "NOT_YET_RELEASED" both map to Sonarr's
    // "continuing" — the show is still emitting episodes from
    // Sonarr's POV.
    assert_eq!(map_status("RELEASING"), "continuing");
    assert_eq!(map_status("NOT_YET_RELEASED"), "continuing");
}

#[test]
fn map_status_finished_to_ended() {
    // AL's "FINISHED" / "FINISHED_AIRING" / "CANCELLED" all map to
    // Sonarr's "ended". Cancelled is bundled with finished because
    // Sonarr has no separate concept for canceled-mid-air; both
    // mean "no more episodes coming."
    assert_eq!(map_status("FINISHED"), "ended");
    assert_eq!(map_status("FINISHED_AIRING"), "ended");
    assert_eq!(map_status("CANCELLED"), "ended");
}

#[test]
fn map_status_is_case_insensitive() {
    // The AL parser sometimes lower-cases the status field on
    // negative-id (Jikan-fallback) rows. Pin the case-insensitivity
    // so a parser tweak doesn't silently break the mapping.
    assert_eq!(map_status("releasing"), "continuing");
    assert_eq!(map_status("Finished"), "ended");
}

#[test]
fn map_status_unknown_defaults_to_continuing() {
    // Defensive default — better to mark an unknown status as
    // "continuing" (Sonarr keeps watching) than as "ended" (Sonarr
    // stops monitoring entirely). New AL status variants get a
    // safe landing without a code change.
    assert_eq!(map_status("HIATUS"), "continuing");
    assert_eq!(map_status(""), "continuing");
}
