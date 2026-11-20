//! Literal substring search.
//!
//! `ILIKE` gives `%` and `_` a meaning of their own, so text typed into a search
//! box reaches SQL as a pattern: `%` matches every row, and a name that contains
//! a `%` cannot be found at all. Every search in the tree escapes through here,
//! which is the only way two searches cannot drift.

/// The escape character the queries declare with `ESCAPE`. It is written there
/// as `E'\\'`, so the meaning does not depend on `standard_conforming_strings`.
pub const ESCAPE: char = '\\';

/// The operator's text as a literal `LIKE` pattern for a substring match.
///
/// The result already carries the surrounding `%`, so a caller binds it as-is
/// and never concatenates wildcards of its own.
pub fn contains(raw: &str) -> String {
    let mut pattern = String::with_capacity(raw.len() + 2);
    pattern.push('%');
    for ch in raw.chars() {
        if ch == ESCAPE || ch == '%' || ch == '_' {
            pattern.push(ESCAPE);
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_the_text_and_escapes_the_three_metacharacters() {
        assert_eq!(contains("flour"), "%flour%");
        assert_eq!(contains("50%"), "%50\\%%");
        assert_eq!(contains("under_score"), "%under\\_score%");
        assert_eq!(contains("back\\slash"), "%back\\\\slash%");
        assert_eq!(contains(""), "%%");
    }

    #[test]
    fn accented_text_is_left_alone() {
        assert_eq!(contains("Lácteos"), "%Lácteos%");
    }
}
