//! `sanitize_for_log` coverage. The helper guards every log line that
//! embeds attacker-supplied text (form usernames on failed logins).
//! Without it, a username probe smuggling terminal escapes or ANSI
//! sequences could clobber an admin's `tail -f`, and a multi-kilobyte
//! probe value would land verbatim in the `logs` table.

use crate::handlers::auth::sanitize_for_log;

#[test]
fn passes_through_plain_ascii_unchanged() {
    assert_eq!(sanitize_for_log("admin"), "admin");
    assert_eq!(
        sanitize_for_log("user.name+tag@example.com"),
        "user.name+tag@example.com"
    );
}

#[test]
fn trims_surrounding_whitespace() {
    // Bare whitespace on either side is a noisy artifact of typed
    // input — strip it before logging.
    assert_eq!(sanitize_for_log("  admin  "), "admin");
    assert_eq!(sanitize_for_log("\t\nhello\r\n"), "hello");
}

#[test]
fn strips_ascii_control_chars() {
    // `\x07` is BEL — would beep the terminal. `\x1b` is ESC, the
    // start of every ANSI escape sequence (color, cursor moves,
    // alternative-screen). A username probe carrying these would
    // wreck a `tail -f` session.
    assert_eq!(sanitize_for_log("hello\x07world"), "helloworld");
    assert_eq!(sanitize_for_log("\x1b[31mRED\x1b[0m"), "[31mRED[0m");
    // Embedded NUL inside the string also gets dropped — it would
    // truncate the column on most logging consumers.
    assert_eq!(sanitize_for_log("a\x00b"), "ab");
}

#[test]
fn caps_at_64_chars_to_block_oversized_probes() {
    // The cap stops a multi-KB probe from landing verbatim in the
    // `logs` table. 64 chars is enough to surface a real username
    // for diagnostics without giving a probe room to embed
    // arbitrary content.
    let long: String = "A".repeat(200);
    let sanitized = sanitize_for_log(&long);
    assert_eq!(sanitized.chars().count(), 64);
    assert!(sanitized.chars().all(|c| c == 'A'));
}

#[test]
fn cap_is_char_aware_not_byte_aware() {
    // Multi-byte UTF-8 chars count as one char each — a 64-emoji
    // username caps at 64 chars (256 bytes), not at 64 bytes (16
    // emojis). The byte-aware variant would also need to respect
    // a UTF-8 boundary or it would panic; char-aware sidesteps
    // both concerns.
    let emojis: String = "🎌".repeat(80);
    let sanitized = sanitize_for_log(&emojis);
    assert_eq!(sanitized.chars().count(), 64);
}

#[test]
fn empty_input_passes_through() {
    assert_eq!(sanitize_for_log(""), "");
    // Whitespace-only collapses to empty after the trim step.
    assert_eq!(sanitize_for_log("   \t\n"), "");
}

#[test]
fn unicode_letters_pass_through_unchanged() {
    // Non-ASCII letters are NOT control chars and shouldn't be
    // stripped — supports usernames in non-Latin scripts. Capped
    // by the 64-char limit but otherwise preserved.
    assert_eq!(sanitize_for_log("ユーザー"), "ユーザー");
    assert_eq!(sanitize_for_log("Müller"), "Müller");
}

#[test]
fn newline_inside_input_is_stripped_not_used_as_separator() {
    // A multi-line probe must collapse to a single line so it can't
    // forge fake log entries by injecting `\n[FAKE LEVEL]`.
    let injected = "real_user\n[INFO] forged log line";
    let sanitized = sanitize_for_log(injected);
    assert!(
        !sanitized.contains('\n'),
        "no newlines should survive: {sanitized:?}"
    );
}
