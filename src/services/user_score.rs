//! Render-time conversion for user-score badges (issue #62).
//!
//! AniList stores a single numeric score per entry; the user picks
//! which display format they want it rendered in (POINT_10, stars,
//! POINT_3 outline smiley faces, etc.). The score-format string
//! lives on `external_accounts.score_format` and is applied at
//! render time rather than write time — that way a user changing
//! their format preference on AL flips every "You: X" badge in
//! Ryokan on the next sync (which re-reads `score_format` along
//! with the watch list) without needing to touch every series row.
//!
//! POINT_3 renders as inline outline-face SVGs (sad/neutral/happy),
//! matching AL's own UI. The other formats are plain text. The
//! [`FormattedUserScore`] enum is the type-safe split — templates
//! call `render_html()` which emits HTML-safe text or SVG markup
//! depending on the variant.
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
///   - `score` is `0.0`, negative, or NaN (AL's "unrated" sentinel
///     covers the first two; NaN is theoretical defense — `(NaN as
///     i64) = 0`, which would render `You: 0` if it slipped through).
///   - `score_format` is empty, indicating no account is currently
///     linked. A residual `user_score` from a previously-linked
///     account would otherwise render through this function's
///     POINT_10 fallback against the wrong formatter (e.g. an AL
///     POINT_100 score of 85 surfacing as "You: 85" through MAL's
///     POINT_10). The unlink path wipes user_score so this branch
///     shouldn't fire in practice, but defense-in-depth keeps a
///     buggy data-state from leaking nonsense badges.
///
/// Returns `Some(formatted)` otherwise — variant per format:
///   - `POINT_3` → `Smiley(Sad|Neutral|Happy)`. The template renders
///     each as an inline outline-face SVG via `Smiley::svg()`.
///   - `POINT_5` → `Text("★★★☆☆")` style (1..=5 filled out of 5).
///   - `POINT_10` (default for unknown formats + MAL) → `Text("8")`.
///   - `POINT_10_DECIMAL` → `Text("8.5")` (one decimal).
///   - `POINT_100` → `Text("85")` (integer 1..=100).
///
/// Unknown non-empty `score_format` strings fall through to
/// `POINT_10` rather than returning `None` — better to show a number
/// than hide the user's score entirely on a future format addition.
pub fn format_user_score(score: Option<f64>, score_format: &str) -> Option<FormattedUserScore> {
    if score_format.is_empty() {
        return None;
    }
    let s = score?;
    if s.is_nan() || s <= 0.0 {
        return None;
    }
    Some(match score_format {
        "POINT_3" => format_point_3(s),
        "POINT_5" => FormattedUserScore::Text(format_point_5(s)),
        "POINT_10_DECIMAL" => FormattedUserScore::Text(format!("{s:.1}")),
        "POINT_100" => FormattedUserScore::Text(format!("{}", s.round() as i64)),
        _ => FormattedUserScore::Text(format!("{}", s.round() as i64)),
    })
}

/// Two-shape display for the rendered user score. Most formats are
/// plain text (numbers, stars). AL's POINT_3 is a 3-way smiley scale
/// that AL itself renders as outlined faces; we mirror that by
/// returning a [`Smiley`] variant the template renders as inline
/// SVG instead of a text glyph.
#[derive(Debug, Clone, PartialEq)]
pub enum FormattedUserScore {
    Text(String),
    Smiley(Smiley),
}

/// AL's POINT_3 scale. Stored on disk as `1.0` / `2.0` / `3.0`; the
/// template branches on this enum to render the matching outline-
/// face SVG. Any value outside `1..=3` falls through to a numeric
/// `Text` render so a future AL scale-shape change doesn't leave
/// the badge blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Smiley {
    Sad,
    Neutral,
    Happy,
}

impl FormattedUserScore {
    /// Render-time helper for templates: HTML-safe string for the
    /// Text variant (numbers + stars are already pre-escaped by
    /// construction), inline SVG markup for the Smiley variant.
    /// Templates use this with the `|safe` filter so the SVG
    /// passes through unescaped.
    pub fn render_html(&self) -> String {
        match self {
            FormattedUserScore::Text(s) => s.clone(),
            FormattedUserScore::Smiley(s) => s.svg().to_string(),
        }
    }
}

impl Smiley {
    /// Inline SVG for the smiley face. Outline-only, sized to sit on
    /// the same baseline as the surrounding badge text. `currentColor`
    /// stroke so it picks up the `.tag-user-score` accent tint without
    /// a separate fill rule. `aria-label` keeps the screen-reader
    /// behavior parity with the text formats.
    pub fn svg(&self) -> &'static str {
        match self {
            Smiley::Sad => SAD_SVG,
            Smiley::Neutral => NEUTRAL_SVG,
            Smiley::Happy => HAPPY_SVG,
        }
    }
}

const SAD_SVG: &str = r#"<svg class="user-score-smiley" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-label="Score: sad"><circle cx="12" cy="12" r="10"/><path d="M16 16s-1.5-2-4-2-4 2-4 2"/><line x1="9" y1="9" x2="9.01" y2="9"/><line x1="15" y1="9" x2="15.01" y2="9"/></svg>"#;
const NEUTRAL_SVG: &str = r#"<svg class="user-score-smiley" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-label="Score: neutral"><circle cx="12" cy="12" r="10"/><line x1="8" y1="15" x2="16" y2="15"/><line x1="9" y1="9" x2="9.01" y2="9"/><line x1="15" y1="9" x2="15.01" y2="9"/></svg>"#;
const HAPPY_SVG: &str = r#"<svg class="user-score-smiley" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-label="Score: happy"><circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2"/><line x1="9" y1="9" x2="9.01" y2="9"/><line x1="15" y1="9" x2="15.01" y2="9"/></svg>"#;

fn format_point_3(s: f64) -> FormattedUserScore {
    let n = s.round() as i64;
    match n {
        1 => FormattedUserScore::Smiley(Smiley::Sad),
        2 => FormattedUserScore::Smiley(Smiley::Neutral),
        3 => FormattedUserScore::Smiley(Smiley::Happy),
        // Out-of-range falls through to numeric so a future AL scale
        // shift doesn't blank the badge.
        _ => FormattedUserScore::Text(format!("{n}")),
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

    fn text(s: &str) -> FormattedUserScore {
        FormattedUserScore::Text(s.into())
    }

    #[test]
    fn point_10_renders_integer() {
        assert_eq!(format_user_score(Some(8.0), "POINT_10"), Some(text("8")));
        // Half-points round to nearest. If the user picked POINT_10
        // we never show "8.5".
        assert_eq!(format_user_score(Some(8.5), "POINT_10"), Some(text("9")));
        assert_eq!(format_user_score(Some(8.4), "POINT_10"), Some(text("8")));
    }

    #[test]
    fn point_10_decimal_renders_one_fraction() {
        assert_eq!(
            format_user_score(Some(8.5), "POINT_10_DECIMAL"),
            Some(text("8.5"))
        );
        assert_eq!(
            format_user_score(Some(7.0), "POINT_10_DECIMAL"),
            Some(text("7.0"))
        );
    }

    #[test]
    fn point_100_renders_integer() {
        assert_eq!(format_user_score(Some(85.0), "POINT_100"), Some(text("85")));
        assert_eq!(
            format_user_score(Some(99.7), "POINT_100"),
            Some(text("100"))
        );
    }

    #[test]
    fn point_3_renders_smiley_variants() {
        // AL's three-point scale is rendered as outlined-face SVGs
        // by the template; the helper returns the Smiley variant so
        // the template can branch on it. Out-of-range falls through
        // to numeric Text.
        assert_eq!(
            format_user_score(Some(1.0), "POINT_3"),
            Some(FormattedUserScore::Smiley(Smiley::Sad))
        );
        assert_eq!(
            format_user_score(Some(2.0), "POINT_3"),
            Some(FormattedUserScore::Smiley(Smiley::Neutral))
        );
        assert_eq!(
            format_user_score(Some(3.0), "POINT_3"),
            Some(FormattedUserScore::Smiley(Smiley::Happy))
        );
        assert_eq!(format_user_score(Some(4.0), "POINT_3"), Some(text("4")));
    }

    #[test]
    fn smiley_svg_contains_currentcolor_stroke() {
        // The badge inherits its color via .tag-user-score; the SVG
        // must use stroke="currentColor" so it picks up the same
        // accent. Pin this so a future SVG redraw doesn't accidentally
        // hardcode a color.
        for s in [Smiley::Sad, Smiley::Neutral, Smiley::Happy] {
            assert!(
                s.svg().contains(r#"stroke="currentColor""#),
                "{:?} SVG must use currentColor stroke",
                s
            );
            assert!(
                s.svg().starts_with("<svg"),
                "{:?} SVG must be a complete <svg> element",
                s
            );
        }
    }

    #[test]
    fn render_html_returns_text_or_svg_per_variant() {
        // Text variant round-trips its inner string verbatim; Smiley
        // variant emits its SVG markup. Templates use `|safe` on the
        // result to let the SVG through.
        assert_eq!(text("8").render_html(), "8");
        let happy = FormattedUserScore::Smiley(Smiley::Happy);
        assert!(happy.render_html().contains("<svg"));
        assert!(happy.render_html().contains("Score: happy"));
    }

    #[test]
    fn point_5_stars() {
        assert_eq!(format_user_score(Some(1.0), "POINT_5"), Some(text("★☆☆☆☆")));
        assert_eq!(format_user_score(Some(3.0), "POINT_5"), Some(text("★★★☆☆")));
        assert_eq!(format_user_score(Some(5.0), "POINT_5"), Some(text("★★★★★")));
        // Out-of-range falls through to numeric.
        assert_eq!(format_user_score(Some(6.0), "POINT_5"), Some(text("6")));
    }

    #[test]
    fn unknown_non_empty_format_falls_through_to_point_10() {
        // Future AL format addition shouldn't blank the badge — show
        // the number until the renderer learns the new shape.
        assert_eq!(format_user_score(Some(8.0), "POINT_NEW"), Some(text("8")));
        assert_eq!(
            format_user_score(Some(8.0), "POINT_FUTURE_42"),
            Some(text("8"))
        );
    }

    #[test]
    fn empty_format_returns_none_to_hide_stale_scores() {
        // Regression for the unlink case: a user unlinks AL while
        // series rows still carry user_score from the previous sync.
        // The handler passes "" for score_format because no account
        // is currently linked. Render must hide the badge — without
        // this the renderer would fall through to POINT_10 and an
        // AL POINT_100 score of 85 would render as "You: 85" through
        // the wrong formatter. (The unlink path also wipes
        // user_score; this guard is defense-in-depth.)
        assert!(format_user_score(Some(85.0), "").is_none());
        assert!(format_user_score(Some(8.5), "").is_none());
    }

    #[test]
    fn nan_score_returns_none() {
        // (NaN as i64) is 0 in Rust, so without an explicit guard a
        // NaN slipping through (corrupt DB, buggy provider) would
        // render as "You: 0" — exactly what the zero-sentinel guard
        // exists to prevent. AL/MAL never send NaN today; the guard
        // is theoretical but cheap.
        assert!(format_user_score(Some(f64::NAN), "POINT_10").is_none());
    }

    #[test]
    fn mal_scores_render_via_point_10_fallback() {
        // MAL doesn't expose a score_format on its API; the sync
        // engine writes `"POINT_10"` for MAL accounts so the
        // renderer treats them the same way.
        assert_eq!(format_user_score(Some(7.0), "POINT_10"), Some(text("7")));
    }
}
