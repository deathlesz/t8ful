//! A small fuzzy matcher shared by the model and session pickers.
//!
//! It is intentionally tiny: callers sort candidates by the score, and those
//! that return `None` are filtered out.

/// Characters that start a new word inside an identifier or a title.
fn is_boundary(character: char) -> bool {
    character.is_whitespace() || matches!(character, '/' | '-' | '_' | ':' | '.')
}

/// Compare chars case-insensitively without assuming ASCII.
fn matches_folded(left: char, right: char) -> bool {
    if left.is_ascii() && right.is_ascii() {
        return left.eq_ignore_ascii_case(&right);
    }

    left == right || left.to_lowercase().eq(right.to_lowercase())
}

/// Score `haystack` against `query` as a case-insensitive subsequence.
///
/// Each matched character earns `+1`, `+2` at a word boundary, or `+3` when it
/// continues the previous match, so contiguous and word-prefix hits rank above
/// scattered ones. Returns `None` when `query` is not a subsequence; an empty
/// query matches everything with a score of zero.
pub fn score(query: &str, haystack: &str) -> Option<i32> {
    let mut expected = query.chars();
    let Some(mut wanted) = expected.next() else {
        return Some(0);
    };

    // Walk the haystack once, pulling the next query character only when the
    // current one matches: no per-candidate allocation, so a fuzzy scan over
    // tens of thousands of paths stays cheap.
    let mut total = 0;
    let mut previous = None;
    let mut last: Option<char> = None;

    for (position, candidate) in haystack.chars().enumerate() {
        if matches_folded(candidate, wanted) {
            let contiguous = previous.is_some_and(|previous| position == previous + 1);
            let boundary = position == 0 || last.is_some_and(is_boundary);

            total += if contiguous {
                3
            } else if boundary {
                2
            } else {
                1
            };
            previous = Some(position);

            match expected.next() {
                Some(next) => wanted = next,
                None => return Some(total),
            }
        }

        last = Some(candidate);
    }

    None
}

/// The best score across several fields, ignoring fields that do not match.
pub fn best<'a>(query: &str, fields: impl IntoIterator<Item = &'a str>) -> Option<i32> {
    fields
        .into_iter()
        .filter_map(|field| score(query, field))
        .max()
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn contiguous_and_word_prefixed_matches_rank_highest() {
        let contiguous = score("gpt", "gpt-4o").expect("match");
        let word_prefixed = score("gpt", "openai/gpt-4o").expect("match");
        let scattered = score("gpt", "great prompt test").expect("match");

        assert!(word_prefixed > scattered, "{word_prefixed} {scattered}");
        assert!(contiguous > scattered, "{contiguous} {scattered}");
        assert_eq!(contiguous, 8, "a contiguous run scores every char");
    }

    #[test]
    fn word_boundaries_outrank_matches_inside_a_word() {
        let boundary = score("cd", "ab-cd").expect("match");
        let inside = score("cd", "abcd").expect("match");

        assert!(boundary > inside, "{boundary} {inside}");
        assert_eq!(boundary, 5);
        assert_eq!(inside, 4);
    }

    #[test]
    fn matching_is_case_insensitive_and_unicode_safe() {
        assert_eq!(score("GPT", "openai/gpt-4o"), score("gpt", "openai/gpt-4o"));
        assert!(score("é", "café").is_some());
        assert!(score("Ü", "über").is_some());
        assert!(score("→", "a→b").is_some());
    }

    #[test]
    fn non_subsequences_are_rejected_and_the_empty_query_matches_all() {
        assert_eq!(score("zz", "gpt-4o"), None);
        assert_eq!(score("gptx", "gpt"), None);
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(score("", ""), Some(0));
    }
}
