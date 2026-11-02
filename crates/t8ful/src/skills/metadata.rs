//! `SKILL.md` frontmatter parsing.
//!
//! The format is a `---` line, a YAML mapping, a closing `---` line, and the
//! instruction body. Only `name` and `description` are interpreted; unknown
//! keys stay untouched so license and tool hints in existing skills work.

use std::path::Path;

use serde::Deserialize;

/// Largest body accepted for one skill. A skill is instructions, not a data
/// set; anything larger is almost certainly a mistake.
pub const MAX_SKILL_BYTES: usize = 1024 * 1024;

/// Longest model-facing description, including the trailing ellipsis.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// Parsed frontmatter plus the instruction body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: Option<String>,
    pub description: String,
    pub body: String,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    name: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    description: Option<serde_yaml_ng::Value>,
}

/// Read and parse `<skill>/SKILL.md`.
pub fn read(path: &Path) -> Result<SkillMetadata, String> {
    // Check before reading so a huge or non-regular file cannot be loaded into
    // memory (or block on a FIFO) just to be rejected afterwards.
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;

    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }

    if metadata.len() > MAX_SKILL_BYTES as u64 {
        return Err(format!(
            "{} is larger than the {MAX_SKILL_BYTES}-byte limit",
            path.display()
        ));
    }

    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let text =
        String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))?;

    parse(&text).map_err(|error| format!("invalid {}: {error}", path.display()))
}

/// Parse frontmatter and body from already-loaded text.
pub fn parse(text: &str) -> Result<SkillMetadata, String> {
    let (yaml, body) = split_frontmatter(text)?;
    let frontmatter: Frontmatter = serde_yaml_ng::from_str(yaml)
        .map_err(|error| format!("frontmatter is not valid YAML: {error}"))?;

    let name = match frontmatter.name {
        None => None,
        Some(serde_yaml_ng::Value::String(raw)) => {
            let trimmed = raw.trim();

            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        Some(other) => {
            return Err(format!(
                "frontmatter `name` must be a string, not {}",
                yaml_kind(&other)
            ));
        }
    };

    let description = match frontmatter.description {
        Some(serde_yaml_ng::Value::String(raw)) => collapse(&raw),
        Some(other) => {
            return Err(format!(
                "frontmatter `description` must be a string, not {}",
                yaml_kind(&other)
            ));
        }
        None => return Err("frontmatter is missing a `description`".to_owned()),
    };

    if description.is_empty() {
        return Err("frontmatter `description` is blank".to_owned());
    }

    Ok(SkillMetadata {
        name,
        description: cap(&description),
        body: body.trim().to_owned(),
    })
}

/// Split the leading `---` block from the body, accepting LF and CRLF.
fn split_frontmatter(text: &str) -> Result<(&str, &str), String> {
    let mut yaml_start = None;
    let mut cursor = 0;

    for line in text.split_inclusive('\n') {
        let content = line.trim_end();

        match yaml_start {
            None => {
                if content != "---" {
                    return Err("SKILL.md must start with a `---` frontmatter block".to_owned());
                }

                cursor += line.len();
                yaml_start = Some(cursor);
            }
            Some(start) => {
                if content == "---" {
                    return Ok((&text[start..cursor], &text[cursor + line.len()..]));
                }

                cursor += line.len();
            }
        }
    }

    Err("SKILL.md has an unterminated `---` frontmatter block".to_owned())
}

/// Collapse all whitespace (including YAML folds and line breaks) to single
/// spaces.
fn collapse(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bound a description so a hostile or careless skill cannot flood the prompt.
fn cap(description: &str) -> String {
    if description.chars().count() <= MAX_DESCRIPTION_CHARS {
        return description.to_owned();
    }

    let mut capped = description
        .chars()
        .take(MAX_DESCRIPTION_CHARS - 1)
        .collect::<String>();

    capped.push('…');

    capped
}

fn yaml_kind(value: &serde_yaml_ng::Value) -> &'static str {
    match value {
        serde_yaml_ng::Value::Null => "null",
        serde_yaml_ng::Value::Bool(_) => "a boolean",
        serde_yaml_ng::Value::Number(_) => "a number",
        serde_yaml_ng::Value::String(_) => "a string",
        serde_yaml_ng::Value::Sequence(_) => "a list",
        serde_yaml_ng::Value::Mapping(_) => "a mapping",
        serde_yaml_ng::Value::Tagged(_) => "a tagged value",
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_DESCRIPTION_CHARS, parse};

    #[test]
    fn plain_frontmatter_yields_name_description_and_body() {
        let skill = parse(
            "---\nname: review\ndescription: Review a change.\n---\n\n# Review\n\nBe careful.\n",
        )
        .expect("parse");

        assert_eq!(skill.name.as_deref(), Some("review"));
        assert_eq!(skill.description, "Review a change.");
        assert_eq!(skill.body, "# Review\n\nBe careful.");
    }

    #[test]
    fn unknown_keys_and_a_missing_name_are_accepted() {
        let skill = parse(
            "---\ndescription: Use this.\nlicense: MIT\nallowed-tools: [read_file]\n---\nbody\n",
        )
        .expect("parse");

        assert_eq!(skill.name, None);
        assert_eq!(skill.description, "Use this.");
    }

    #[test]
    fn folded_literal_and_quoted_descriptions_are_collapsed() {
        let folded = parse(
            "---\ndescription: >-\n  Review a change for\n  correctness and style.\n---\nbody\n",
        )
        .expect("folded description");

        assert_eq!(
            folded.description,
            "Review a change for correctness and style."
        );

        let literal = parse("---\ndescription: |\n  Line one\n  line two\n---\nbody\n")
            .expect("literal description");

        assert_eq!(literal.description, "Line one line two");

        let quoted =
            parse("---\ndescription: \"quoted    text\"\n---\nbody\n").expect("quoted description");

        assert_eq!(quoted.description, "quoted text");
    }

    #[test]
    fn crlf_files_parse_the_same_way() {
        let skill =
            parse("---\r\nname: review\r\ndescription: Review.\r\n---\r\nbody\r\n").expect("parse");

        assert_eq!(skill.name.as_deref(), Some("review"));
        assert_eq!(skill.description, "Review.");
        assert_eq!(skill.body, "body");
    }

    #[test]
    fn missing_or_unterminated_frontmatter_is_rejected() {
        assert!(parse("# no frontmatter\n").is_err());
        assert!(parse("---\nname: review\ndescription: Review.\n").is_err());
        assert!(parse("---\nname: review\ndescription: Review.\n--- ").is_ok());
    }

    #[test]
    fn a_missing_blank_or_non_string_description_is_rejected() {
        for text in [
            "---\nname: review\n---\nbody\n",
            "---\ndescription: \"   \"\n---\nbody\n",
            "---\ndescription: 42\n---\nbody\n",
            "---\ndescription: [a, b]\n---\nbody\n",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_non_string_name_is_rejected_but_an_invalid_component_is_left_to_discovery() {
        assert!(parse("---\nname: 7\ndescription: Review.\n---\nbody\n").is_err());

        let skill = parse("---\nname: ../escape\ndescription: Review.\n---\nbody\n")
            .expect("the component check belongs to discovery");

        assert_eq!(skill.name.as_deref(), Some("../escape"));
    }

    #[test]
    fn an_oversized_skill_file_is_rejected() {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let path = directory.path().join("SKILL.md");
        let mut contents = String::from("---\nname: review\ndescription: Review.\n---\n");

        contents.push_str(&"x".repeat(super::MAX_SKILL_BYTES));

        std::fs::write(&path, contents).expect("skill file");

        let error = super::read(&path).expect_err("oversized");

        assert!(error.contains("larger than"), "{error}");
    }

    #[test]
    fn whitespace_collapses_and_long_descriptions_are_capped() {
        let long = format!(
            "---\ndescription: {}\n---\nbody\n",
            "word ".repeat(MAX_DESCRIPTION_CHARS)
        );
        let skill = parse(&long).expect("parse");

        assert_eq!(
            skill.description.chars().count(),
            MAX_DESCRIPTION_CHARS,
            "the ellipsis counts against the cap"
        );
        assert!(skill.description.ends_with('…'));

        let short = parse("---\ndescription: \"  a   b  \"\n---\nbody\n").expect("parse");

        assert_eq!(short.description, "a b");
    }
}
