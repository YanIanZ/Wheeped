//! Text rules the Java original applies to manifests, pointers and indexes.
//!
//! These files are parsed with `String.split`, `String.trim` and a digits-only check. The exact
//! rules decide which damaged files are refused, so they are kept here rather than approximated
//! with Rust's own `split` and `trim`.

/// Splits like Java's `String.split(separator)`: trailing empty fields are dropped, and a text
/// without the separator is returned whole, also when it is empty.
pub fn split_java(text: &str, separator: char) -> Vec<&str> {
    if !text.contains(separator) {
        return vec![text];
    }
    let mut parts: Vec<&str> = text.split(separator).collect();
    while parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    parts
}

/// Trims like Java's `String.trim`: every character up to and including a space, at both ends.
pub fn trim_java(text: &str) -> &str {
    text.trim_matches(|c: char| c <= ' ')
}

/// A generation number, blob length or format number: one to eighteen ASCII digits.
///
/// The Java check uses `Character.isDigit`, which also takes decimal digits of other scripts. No
/// writer produces those, and this port refuses them.
pub fn parse_number(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 18 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Whether a field has the length of a SHA-256 in hex, counted as Java counts a string's length.
pub fn has_sha256_length(text: &str) -> bool {
    text.encode_utf16().count() == 64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_drops_trailing_empty_fields_only() {
        assert_eq!(split_java("a\tb\t", '\t'), ["a", "b"]);
        assert_eq!(split_java("a\t\tb", '\t'), ["a", "", "b"]);
        assert_eq!(split_java("\ta", '\t'), ["", "a"]);
        assert!(split_java("\t\t", '\t').is_empty());
        assert_eq!(split_java("", '\t'), [""]);
        assert_eq!(split_java("whole", '\t'), ["whole"]);
    }

    #[test]
    fn trim_takes_control_characters_but_not_other_whitespace() {
        assert_eq!(trim_java(" \t\n7\u{1}"), "7");
        assert_eq!(trim_java("\u{a0}7\u{a0}"), "\u{a0}7\u{a0}");
    }

    #[test]
    fn a_number_is_ascii_digits_of_bounded_length() {
        assert_eq!(parse_number("0"), Some(0));
        assert_eq!(
            parse_number("999999999999999999"),
            Some(999_999_999_999_999_999)
        );
        assert_eq!(parse_number("1000000000000000000"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("+1"), None);
        assert_eq!(parse_number("-1"), None);
        assert_eq!(parse_number("\u{661}"), None);
    }
}
