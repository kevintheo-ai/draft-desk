//! Character guards: line breaks and invisible or dangerous characters.

/// Every character Python's `str.splitlines()` treats as a line break. Any of them in a header field would allow
/// header injection.
pub const LINEBREAKS: &[char] = &[
    '\n', '\r', '\u{0B}', '\u{0C}', '\u{1C}', '\u{1D}', '\u{1E}', '\u{85}', '\u{2028}', '\u{2029}',
];

/// Invisible or dangerous: C0 controls except tab, line feed and carriage return, DEL, C1 controls, the Arabic letter
/// mark (U+061C), zero-width and bidi marks (U+200B-U+200F), bidi embeddings and overrides (U+202A-U+202E), word joiner and invisible operators
/// (U+2060-U+2064), bidi isolates (U+2066-U+2069), the byte order mark and the Unicode tag block (U+E0000-U+E007F).
pub fn is_forbidden(c: char) -> bool {
    matches!(c as u32,
        0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F..=0x9F
        | 0x061C | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x2069
        | 0xFEFF | 0xE0000..=0xE007F)
}

pub fn has_linebreak(s: &str) -> bool {
    s.contains(LINEBREAKS)
}

/// Header fields: tab is allowed, as in the Python version.
pub fn has_forbidden(s: &str) -> bool {
    s.chars().any(is_forbidden)
}

/// Removes forbidden characters and carriage returns; keeps line feeds and tabs.
pub fn strip_forbidden(s: &str) -> String {
    s.chars()
        .filter(|&c| c != '\r' && !is_forbidden(c))
        .collect()
}

/// Every forbidden character in a body, as (byte offset, character), so the window can show and remove them.
pub fn body_problems(body: &str) -> Vec<(usize, char)> {
    body.char_indices()
        .filter(|&(_, c)| is_forbidden(c))
        .collect()
}

/// Every forbidden character as (position, length) in UTF-16 code units, the unit of `NSString` ranges.
pub fn forbidden_utf16(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    for c in s.chars() {
        let n = c.len_utf16();
        if is_forbidden(c) {
            out.push((pos, n));
        }
        pos += n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_utf16_counts_surrogate_pairs() {
        assert_eq!(forbidden_utf16("a\u{1F600}\u{202E}b"), vec![(3, 1)]);
        assert_eq!(forbidden_utf16("\u{E0041}x\u{200B}"), vec![(0, 2), (3, 1)]);
        assert!(forbidden_utf16("plain\n\ttext \u{fc}").is_empty());
    }

    #[test]
    fn linebreak_chars_match_python() {
        for c in [
            '\n', '\r', '\u{0B}', '\u{0C}', '\u{1C}', '\u{1D}', '\u{1E}', '\u{85}', '\u{2028}',
            '\u{2029}',
        ] {
            assert!(has_linebreak(&format!("a{c}b")), "{:?}", c);
        }
        for c in [' ', '\t', '\u{A0}'] {
            assert!(!has_linebreak(&format!("a{c}b")), "{:?}", c);
        }
    }

    #[test]
    fn strip_removes_rtl_override() {
        assert_eq!(strip_forbidden("ab\u{202E}cd"), "abcd");
    }

    #[test]
    fn strip_keeps_newline_and_tab() {
        assert_eq!(strip_forbidden("a\nb\tc"), "a\nb\tc");
        assert_eq!(strip_forbidden("a\r\nb"), "a\nb");
    }

    #[test]
    fn strip_removes_bidi_zero_width_c0_c1_del() {
        let s = "x\u{200B}\u{200F}\u{202A}\u{2066}\u{2069}\u{FEFF}\u{00}\u{07}\u{1F}\u{7F}\u{80}\u{9F}y";
        assert_eq!(strip_forbidden(s), "xy");
        assert!(has_forbidden(s));
    }

    #[test]
    fn strip_keeps_umlauts() {
        assert_eq!(strip_forbidden("Grüße, Müller ß €"), "Grüße, Müller ß €");
        assert!(!has_forbidden("Grüße, Müller ß € \t"));
    }

    #[test]
    fn strip_removes_tag_block_and_word_joiner() {
        let s = "a\u{E0041}\u{E007F}\u{2060}\u{2064}\u{E0000}\u{061C}b";
        assert_eq!(strip_forbidden(s), "ab");
        // Neighbours of the ranges stay.
        assert!(!is_forbidden('\u{2065}'));
        assert!(!is_forbidden('\u{E0080}'));
        assert!(!is_forbidden('\u{205F}'));
    }

    #[test]
    fn tab_lf_cr_are_not_forbidden() {
        assert!(!is_forbidden('\t'));
        assert!(!is_forbidden('\n'));
        assert!(!is_forbidden('\r'));
    }

    #[test]
    fn body_problems_reports_byte_offsets() {
        assert_eq!(
            body_problems("ä\u{202E}b\u{200B}"),
            vec![(2, '\u{202E}'), (6, '\u{200B}')]
        );
        assert!(body_problems("Hallo\r\nWelt\t").is_empty());
    }
}
