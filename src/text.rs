//! Shared text helpers.

/// Truncate to `max` characters, ending with an ellipsis when cut. Counts
/// chars, never bytes: byte slicing panics on a multi-byte boundary, and the
/// inputs here (API error messages, revisions, container names) can carry
/// arbitrary UTF-8.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    match max {
        0 => String::new(),
        _ => {
            let mut t: String = s.chars().take(max - 1).collect();
            t.push('…');
            t
        }
    }
}

/// Collapse every run of whitespace, line breaks included, into one space, so
/// a multi-line API message reads as a single line that the view can wrap.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_joins_lines_and_squeezes_whitespace() {
        assert_eq!(one_line("  a\n  b\tc\r\n"), "a b c");
        assert_eq!(one_line(""), "");
    }

    #[test]
    fn short_strings_pass_through() {
        assert_eq!(ellipsize("abc", 3), "abc");
        assert_eq!(ellipsize("", 5), "");
    }

    #[test]
    fn long_strings_end_with_ellipsis_at_max_chars() {
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abcdef", 4).chars().count(), 4);
    }

    #[test]
    fn zero_max_is_empty() {
        assert_eq!(ellipsize("abc", 0), "");
    }

    #[test]
    fn multibyte_input_never_panics() {
        // Regression: a byte-sliced truncation panicked when byte 59 fell
        // inside a multi-byte sequence.
        let s = "é".repeat(80);
        assert_eq!(ellipsize(&s, 60).chars().count(), 60);
        assert_eq!(ellipsize("αβγδε", 3), "αβ…");
    }
}
