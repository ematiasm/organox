//! Text bounds shared by the layers that store free text.
//!
//! A column is `text` because the operator's note has no fixed shape, not
//! because the row may grow without limit: a header or a note that arrives
//! oversized is truncated at the boundary, never rejected. Refusing a purchase
//! because somebody pasted a paragraph into the note field would lose the
//! document to protect a storage detail.

/// Longest note kept on a ledger row or a document header.
pub const MAX_NOTE_LEN: usize = 500;

/// Longest captured `User-Agent` kept on a session row.
///
/// Browsers send well under 256 bytes; the bound exists so a client cannot
/// decide how large the session table grows.
pub const MAX_USER_AGENT_LEN: usize = 512;

/// Cuts `value` to at most `max` characters, on a character boundary.
///
/// `chars().take(max)` on a `String` would allocate; slicing by character
/// index keeps the caller's borrow and never splits a multi-byte character.
pub fn truncate_chars(value: &str, max: usize) -> &str {
    value
        .char_indices()
        .nth(max)
        .map_or(value, |(cut, _)| &value[..cut])
}

/// Trims a note and bounds it, treating blank as absent.
///
/// Truncate rather than reject: a long note is no reason to lose the movement
/// or the document being recorded.
pub fn truncate_note(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, MAX_NOTE_LEN).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // Four bytes per character: a byte-based cut would panic or split.
        let value = "🚚".repeat(10);
        assert_eq!(truncate_chars(&value, 3), "🚚🚚🚚");
        assert_eq!(truncate_chars(&value, 10), value);
        assert_eq!(truncate_chars(&value, 99), value);
        assert_eq!(truncate_chars(&value, 0), "");
    }

    #[test]
    fn a_note_keeps_its_bound_and_blank_notes_stay_absent() {
        assert_eq!(truncate_note(None), None);
        assert_eq!(truncate_note(Some("   ")), None);
        assert_eq!(
            truncate_note(Some("  invoice 42  ")).as_deref(),
            Some("invoice 42")
        );

        let long = "x".repeat(MAX_NOTE_LEN + 100);
        assert_eq!(
            truncate_note(Some(&long)).unwrap().chars().count(),
            MAX_NOTE_LEN
        );
    }
}
