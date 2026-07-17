//! One shared sanitizer for terminal-bound untrusted text (task titles/bodies/notes, branch
//! names, board message bodies/senders).
//!
//! The task and board logs are replayed forever, so a poisoned record can never be cured at the
//! send path — every *reader* strips control characters itself (comms canon: "every consumer
//! sanitizes untrusted message text itself"). One ESC sequence in a task title would otherwise
//! corrupt every terminal that ever runs `cv task list`, eternally.
//!
//! Scope decision: this is applied at **terminal render sites only** (CLI print paths, cockpit
//! rows). cvd serves JSON, where escaping is the transport's job — serde_json escapes control
//! characters into harmless `\uXXXX` — so JSON responses stay raw and lossless for foreign
//! clients to sanitize at *their* render sites.

use std::borrow::Cow;

/// Make `s` safe for a one-line terminal row: ESC-initiated sequences (CSI, OSC, DCS, SOS, PM,
/// APC) are dropped entirely (parameters and all), `\n`/`\t`/`\r` become single spaces (render
/// sites are one-line rows), and every other control character (C0, DEL, C1) is removed.
/// Returns the input borrowed when it is already clean — the overwhelmingly common case.
pub fn sanitize_line(s: &str) -> Cow<'_, str> {
    if !s.chars().any(char::is_control) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' | '\r' => out.push(' '),
            '\u{1b}' => skip_escape_sequence(&mut chars),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Consume the remainder of an ESC-initiated sequence (the ESC itself was already consumed), so
/// the sequence's *payload* (colors, window titles, arbitrary OSC bytes) never reaches the
/// terminal as text.
fn skip_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        // CSI: `ESC [`, then parameter/intermediate bytes until a final byte in `@`..=`~`.
        Some('[') => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        // String-carrying sequences (OSC / DCS / SOS / PM / APC): terminated by BEL or ST
        // (`ESC \`); a stray ESC aborts the string either way.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\u{07}' {
                    break;
                }
                if c == '\u{1b}' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        // Two-char sequence (`ESC c`, `ESC 7`, ...) — the second char was just consumed — or a
        // trailing bare ESC (None): nothing more to skip.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_line;
    use std::borrow::Cow;

    #[test]
    fn clean_text_passes_through_borrowed() {
        let s = "fix the zebrafish migration ✦ (rev 2)";
        assert!(matches!(sanitize_line(s), Cow::Borrowed(b) if b == s));
    }

    #[test]
    fn strips_osc_title_bomb_and_csi_colors() {
        // OSC terminated by BEL, then a raw CSI color sequence.
        assert_eq!(sanitize_line("evil\u{1b}]0;pwned\u{7}task"), "eviltask");
        assert_eq!(sanitize_line("\u{1b}[31mred\u{1b}[0m"), "red");
        // OSC terminated by ST (ESC \).
        assert_eq!(sanitize_line("\u{1b}]0;x\u{1b}\\done"), "done");
        // Multi-parameter CSI.
        assert_eq!(sanitize_line("a\u{1b}[1;31;4mb"), "ab");
    }

    #[test]
    fn structural_whitespace_becomes_single_spaces() {
        assert_eq!(sanitize_line("a\nb\tc\r\nd"), "a b c  d");
    }

    #[test]
    fn drops_bare_controls_del_and_lone_esc() {
        assert_eq!(sanitize_line("a\u{8}b\u{7f}c\u{1b}"), "abc");
        // Unterminated OSC eats to end of string rather than leaking payload.
        assert_eq!(sanitize_line("x\u{1b}]0;never terminated"), "x");
        // Two-char ESC sequence: both chars dropped.
        assert_eq!(sanitize_line("a\u{1b}cb"), "ab");
    }
}
