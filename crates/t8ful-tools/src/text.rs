//! Text fitting shared by tools that return a bounded answer.

/// Fit `text` into `max_bytes`, trimming on a character boundary.
///
/// The returned flag is true when anything was cut, so the caller can report a
/// truncation instead of letting the model treat a clipped answer as complete.
pub(crate) fn truncate(mut text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }

    const NOTE: &str = "\n(truncated to fit the output limit)";
    // Never exceed the caller's limit, even when the note alone would.
    let note = if max_bytes > NOTE.len() { NOTE } else { "" };
    let budget = max_bytes - note.len();
    let mut cut = budget;

    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }

    text.truncate(cut);
    text.push_str(note);

    (text, true)
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn text_within_the_limit_is_untouched() {
        let (text, truncated) = truncate("hello".to_owned(), 5);

        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn a_trimmed_answer_keeps_a_character_boundary_and_says_so() {
        let (text, truncated) = truncate("é".repeat(100), 120);

        assert!(truncated);
        assert!(text.len() <= 120, "{}", text.len());
        assert!(text.is_char_boundary(text.len()));
        assert!(text.contains("truncated"));
    }

    #[test]
    fn a_limit_smaller_than_the_note_still_holds() {
        let (text, truncated) = truncate("abcdefghij".to_owned(), 5);

        assert!(truncated);
        assert!(text.len() <= 5, "{text:?}");
    }
}
