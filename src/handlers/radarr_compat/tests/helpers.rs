//! Pure-helper coverage for the Radarr shim. Radarr's nested
//! `RadarrRatings` shape duplicates the score across both `imdb`
//! and `tmdb` slots — Seerr reads whichever slot fits its render
//! path, so populating only one would render a missing-rating
//! state in some Seerr versions. Pin both slots.

use crate::handlers::radarr_compat::helpers::{map_status, ratings_from_score};

// ── ratings_from_score ───────────────────────────────────────────────

#[test]
fn ratings_from_score_none_zeros_both_slots() {
    let r = ratings_from_score(None);
    assert_eq!(r.imdb.value, 0.0);
    assert_eq!(r.imdb.votes, 0);
    assert_eq!(r.tmdb.value, 0.0);
    assert_eq!(r.tmdb.votes, 0);
}

#[test]
fn ratings_from_score_zero_or_negative_zeros_both_slots() {
    for s in [Some(0), Some(-1)] {
        let r = ratings_from_score(s);
        assert_eq!(r.imdb.value, 0.0);
        assert_eq!(r.tmdb.value, 0.0);
    }
}

#[test]
fn ratings_from_score_divides_by_ten_and_mirrors_to_both_slots() {
    // The same value lands in both slots so Seerr renders a
    // rating regardless of which slot it reads.
    let r = ratings_from_score(Some(85));
    assert_eq!(r.imdb.value, 8.5);
    assert_eq!(r.tmdb.value, 8.5);
}

#[test]
fn ratings_from_score_rating_type_user_in_both_slots() {
    // The rating-type label is hardcoded "user" — Radarr uses this
    // to distinguish IMDB's site rating from a per-user score; we
    // only have the AL community average, which is closest to a
    // user-rating shape.
    let r = ratings_from_score(Some(75));
    assert_eq!(r.imdb.rating_type, "user");
    assert_eq!(r.tmdb.rating_type, "user");
}

// ── map_status ───────────────────────────────────────────────────────

#[test]
fn map_status_releasing_to_announced() {
    // Radarr's vocabulary differs from Sonarr's — movies use
    // "announced" / "released" rather than "continuing" / "ended".
    assert_eq!(map_status("RELEASING"), "announced");
    assert_eq!(map_status("NOT_YET_RELEASED"), "announced");
}

#[test]
fn map_status_finished_to_released() {
    assert_eq!(map_status("FINISHED"), "released");
    assert_eq!(map_status("FINISHED_AIRING"), "released");
    assert_eq!(map_status("CANCELLED"), "released");
}

#[test]
fn map_status_is_case_insensitive() {
    assert_eq!(map_status("releasing"), "announced");
    assert_eq!(map_status("Finished"), "released");
}

#[test]
fn map_status_unknown_defaults_to_released() {
    // Movies default to "released" — for an unknown AL status,
    // assume the movie is out (Radarr's "released" state). The
    // Sonarr side defaults to "continuing" because TV is more
    // forgiving of an "in progress" assumption; the Radarr side
    // leans the other way because most anime "movies" tracked
    // here are theatrical releases that have already premiered.
    assert_eq!(map_status("HIATUS"), "released");
    assert_eq!(map_status(""), "released");
}
