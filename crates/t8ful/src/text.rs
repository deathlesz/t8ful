//! Terminal-safe projection of untrusted text.
//!
//! Model output, file contents, skill metadata, and session titles can carry
//! control characters. The TUI's cell buffer and `--format json` neutralize
//! them; the text renderer must do the same before writing to a terminal.

/// Escape every control character except newline and tab.
pub fn terminal_safe(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());

    for character in text.chars() {
        match character {
            '\n' | '\t' => safe.push(character),
            control if control.is_control() => push_escape(&mut safe, control),
            other => safe.push(other),
        }
    }

    safe
}

/// Escape every control character for a single-line listing row.
pub fn terminal_safe_line(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());

    for character in text.chars() {
        if character.is_control() {
            push_escape(&mut safe, character);
        } else {
            safe.push(character);
        }
    }

    safe
}

/// Render a control character as a visible `\xNN` or `\u{NNNN}` spelling.
fn push_escape(out: &mut String, character: char) {
    let code = character as u32;

    if code <= 0xFF {
        out.push_str(&format!("\\x{code:02X}"));
    } else {
        out.push_str(&format!("\\u{{{code:04X}}}"));
    }
}

#[cfg(test)]
mod tests {
    use super::{terminal_safe, terminal_safe_line};

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(terminal_safe("hello world"), "hello world");
        assert_eq!(terminal_safe_line("hello world"), "hello world");
    }

    #[test]
    fn multiline_projection_keeps_newlines_and_tabs() {
        assert_eq!(
            terminal_safe("line one\n\tindented"),
            "line one\n\tindented"
        );
    }

    #[test]
    fn every_other_control_character_gets_a_visible_spelling() {
        for (input, expected) in [
            ("\u{1b}]0;title\u{7}", "\\x1B]0;title\\x07"),
            ("carriage\rreturn", "carriage\\x0Dreturn"),
            ("c1 \u{9b}2J", "c1 \\x9B2J"),
            ("delete \u{7f}", "delete \\x7F"),
            ("unicode \u{85}", "unicode \\x85"),
        ] {
            assert_eq!(terminal_safe(input), expected, "{input:?}");
        }
    }

    #[test]
    fn line_projection_also_escapes_newlines_and_tabs() {
        assert_eq!(
            terminal_safe_line("one\ntwo\tthree\r"),
            "one\\x0Atwo\\x09three\\x0D"
        );
    }
}
