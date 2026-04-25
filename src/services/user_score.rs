//! Render-time conversion for user-score badges (issue #62 PR C).
//!
//! AniList stores a single numeric score per entry; the user picks
//! which display format they want it rendered in (POINT_10, stars,
//! smileys, etc.). The score-format string lives on
//! `external_accounts.score_format` and is applied at render time
//! rather than write time — that way a user changing their format
//! preference on AL flips every "You: X" badge in Ryokan on the
//! next sync (which re-reads `score_format` along with the watch
//! list) without needing to touch every series row.
//!
//! MAL's API always returns 0..=10 integers; we treat MAL the same
//! as AL's POINT_10. The unrated-on-AL sentinel is `0.0`, and AL's
//! own UI hides "0" — we mirror that by returning `None` from
//! [`format_user_score`] for any non-positive value.

/// Render a `series.user_score` value for badge display, applying
/// the user's chosen `score_format` from
/// `external_accounts.score_format`.
///
/// Returns `None` when:
///   - `score` is `None` (no linked account, or the column is NULL).
///   - `score` is `0.0` or negative (AL's "unrated" sentinel — never
///     show "You: 0" because that's a real AL score the user didn't
///     pick).
///
/// Returns `Some(display_string)` otherwise — formatted per the
/// score-format rules:
///   - `POINT_3` → `:(` (1) / `:|` (2) / `:)` (3). AL's smiley scale.
///   - `POINT_5` → `★★★☆☆` style (1..=5 filled stars out of 5).
///   - `POINT_10` (default for unknown formats + MAL) → `8` (integer).
///   - `POINT_10_DECIMAL` → `8.5` (one decimal).
///   - `POINT_100` → `85` (integer 1..=100).
///
/// Unknown `score_format` strings fall through to `POINT_10` rather
/// than returning `None` — better to show a number than hide the
/// user's score entirely on a future format addition.
pub fn format_user_score(score: Option<f64>, score_format: &str) -> Option<String> {
    let s = score?;
    if s <= 0.0 {
        return None;
    }
    Some(match score_format {
        "POINT_3" => format_point_3(s),
        "POINT_5" => format_point_5(s),
        "POINT_10_DECIMAL" => format!("{s:.1}"),
        "POINT_100" => format!("{}", s.round() as i64),
        _ => format!("{}", s.round() as i64),
    })
}

/// AL's three-point smiley scale. Stored as 1.0/2.0/3.0; rendered as
/// the corresponding glyph. A value outside that range falls through
/// to a numeric render so a future scale change doesn't leave the
/// badge blank.
fn format_point_3(s: f64) -> String {
    let n = s.round() as i64;
    match n {
        1 => ":(".to_string(),
        2 => ":|".to_string(),
        3 => ":)".to_string(),
        _ => format!("{n}"),
    }
}

/// AL's five-star scale. Filled stars + empty stars to 5 total.
/// Out-of-range values fall through to a numeric render.
fn format_point_5(s: f64) -> String {
    let n = s.round() as i64;
    if !(1..=5).contains(&n) {
        return format!("{n}");
    }
    let filled = n as usize;
    let empty = 5 - filled;
    format!("{}{}", "★".repeat(filled), "☆".repeat(empty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_score_returns_none() {
        // No linked account / NULL column → no badge.
        assert!(format_user_score(None, "POINT_10").is_none());
    }

    #[test]
    fn zero_score_returns_none_for_every_format() {
        // AL's "unrated" sentinel — must never render as "You: 0".
        // Cover every format so a renderer that handles one branch
        // wrong gets caught.
        for fmt in [
            "POINT_3",
            "POINT_5",
            "POINT_10",
            "POINT_10_DECIMAL",
            "POINT_100",
            "GARBAGE",
        ] {
            assert!(
                format_user_score(Some(0.0), fmt).is_none(),
                "format {fmt} must hide score 0"
            );
        }
    }

    #[test]
    fn negative_scores_return_none() {
        // Belt-and-braces: a buggy provider response with a negative
        // value should also hide rather than render a confusing badge.
        assert!(format_user_score(Some(-1.0), "POINT_10").is_none());
        assert!(format_user_score(Some(-99.0), "POINT_100").is_none());
    }

    #[test]
    fn point_10_renders_integer() {
        assert_eq!(format_user_score(Some(8.0), "POINT_10"), Some("8".into()));
        // Half-points (an AL UI quirk for the integer scale) round
        // to nearest. The user's chosen format is what matters; if
        // they picked POINT_10 we never show "8.5".
        assert_eq!(format_user_score(Some(8.5), "POINT_10"), Some("9".into()));
        assert_eq!(format_user_score(Some(8.4), "POINT_10"), Some("8".into()));
    }

    #[test]
    fn point_10_decimal_renders_one_fraction() {
        assert_eq!(
            format_user_score(Some(8.5), "POINT_10_DECIMAL"),
            Some("8.5".into())
        );
        assert_eq!(
            format_user_score(Some(7.0), "POINT_10_DECIMAL"),
            Some("7.0".into())
        );
    }

    #[test]
    fn point_100_renders_integer() {
        assert_eq!(
            format_user_score(Some(85.0), "POINT_100"),
            Some("85".into())
        );
        assert_eq!(
            format_user_score(Some(99.7), "POINT_100"),
            Some("100".into())
        );
    }

    #[test]
    fn point_3_smileys() {
        assert_eq!(format_user_score(Some(1.0), "POINT_3"), Some(":(".into()));
        assert_eq!(format_user_score(Some(2.0), "POINT_3"), Some(":|".into()));
        assert_eq!(format_user_score(Some(3.0), "POINT_3"), Some(":)".into()));
        // Out-of-range falls through — never blank.
        assert_eq!(format_user_score(Some(4.0), "POINT_3"), Some("4".into()));
    }

    #[test]
    fn point_5_stars() {
        assert_eq!(
            format_user_score(Some(1.0), "POINT_5"),
            Some("★☆☆☆☆".into())
        );
        assert_eq!(
            format_user_score(Some(3.0), "POINT_5"),
            Some("★★★☆☆".into())
        );
        assert_eq!(
            format_user_score(Some(5.0), "POINT_5"),
            Some("★★★★★".into())
        );
        // Out-of-range falls through to numeric.
        assert_eq!(format_user_score(Some(6.0), "POINT_5"), Some("6".into()));
    }

    #[test]
    fn unknown_format_falls_through_to_point_10() {
        // Future AL format addition shouldn't blank the badge — show
        // the number until the renderer learns the new shape. The
        // test pins the fallback so a refactor that returns None on
        // unknown breaks visibly.
        assert_eq!(format_user_score(Some(8.0), "POINT_NEW"), Some("8".into()));
        assert_eq!(format_user_score(Some(8.0), ""), Some("8".into()));
    }

    #[test]
    fn mal_scores_render_via_point_10_fallback() {
        // MAL doesn't expose a score_format on its API; the sync
        // engine writes `"POINT_10"` for MAL accounts so the
        // renderer treats them the same way. Pin the integer render
        // to confirm the contract.
        assert_eq!(format_user_score(Some(7.0), "POINT_10"), Some("7".into()));
    }
}
