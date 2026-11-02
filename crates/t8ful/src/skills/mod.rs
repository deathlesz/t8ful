//! Skill discovery, metadata, and explicit activation support.
//!
//! Skills are instructions, not code: t8ful reads `SKILL.md` and lists supporting
//! files, but never runs anything from a skill repository on its own.

pub mod discover;
pub mod install;
pub mod metadata;

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::SkillSource;
use t8ful_tools::skill::{SkillDocument, SkillLibrary, SkillSummary};

/// One skill found under a discovery root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkill {
    pub name: String,
    pub description: String,
    pub directory: PathBuf,
    pub source: SkillSource,
    pub managed: Option<ManagedSkill>,
}

/// Install metadata recorded in `.t8ful-skill.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManagedSkill {
    #[serde(rename = "source")]
    pub source_url: String,
    #[serde(default)]
    pub revision: Option<String>,
    pub commit: String,
    #[serde(default)]
    pub subdir: Option<String>,
    #[serde(default)]
    pub installed_at: u64,
}

/// A marker written into a directory installed by `t8ful skills install`.
#[derive(Debug, Serialize)]
pub(crate) struct InstallMarker<'a> {
    pub version: u32,
    pub source: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<&'a str>,
    pub commit: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir: Option<&'a str>,
    pub installed_at: u64,
}

/// The discovered skills plus any warnings produced while scanning.
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    skills: Vec<DiscoveredSkill>,
    warnings: Vec<String>,
}

impl SkillCatalog {
    /// Scan every root in precedence order.
    pub fn discover(roots: &[crate::config::SkillRoot]) -> Self {
        let (skills, warnings) = discover::discover(roots);

        Self { skills, warnings }
    }

    pub fn skills(&self) -> &[DiscoveredSkill] {
        &self.skills
    }

    pub fn get(&self, name: &str) -> Option<&DiscoveredSkill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Load a skill's instructions and supporting-file listing on demand.
    pub fn load(&self, name: &str) -> Result<SkillDocument, String> {
        let skill = self
            .get(name)
            .ok_or_else(|| unknown_skill_message(name, &self.skills))?;
        let metadata = metadata::read(&skill.directory.join("SKILL.md"))?;
        let (files, truncated) = discover::supporting_files(&skill.directory);

        Ok(SkillDocument {
            name: skill.name.clone(),
            directory: skill.directory.clone(),
            body: metadata.body,
            files,
            truncated,
        })
    }
}

fn unknown_skill_message(name: &str, skills: &[DiscoveredSkill]) -> String {
    if skills.is_empty() {
        return format!("unknown skill `{name}`: no skills are installed");
    }

    let mut available = skills
        .iter()
        .take(50)
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    if skills.len() > 50 {
        available.push_str(", …");
    }

    format!("unknown skill `{name}`; available: {available}")
}

impl SkillLibrary for SkillCatalog {
    fn summaries(&self) -> Vec<SkillSummary> {
        self.skills
            .iter()
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect()
    }

    fn load(&self, name: &str) -> Result<SkillDocument, String> {
        Self::load(self, name)
    }
}

#[cfg(test)]
pub(crate) fn test_catalog(skills: Vec<DiscoveredSkill>) -> SkillCatalog {
    SkillCatalog {
        skills,
        warnings: Vec::new(),
    }
}

/// Whether `name` is usable as a skill directory name.
///
/// A skill name is one ordinary path component: no separators, no NUL, no
/// `.`/`..`, and no leading dot (which would hide it from discovery).
pub(crate) fn is_valid_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.contains('/') || name.contains('\0') {
        return false;
    }

    let mut components = Path::new(name).components();

    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::{SkillCatalog, is_valid_name};

    #[test]
    fn skill_names_are_single_ordinary_components() {
        for valid in ["review", "code-review", "with space", "ümlaut"] {
            assert!(is_valid_name(valid), "{valid}");
        }

        for invalid in ["", ".", "..", ".hidden", "../escape", "a/b", "a\0b"] {
            assert!(!is_valid_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn an_empty_catalog_reports_no_skills() {
        let catalog = SkillCatalog::default();
        let error = catalog.load("review").expect_err("missing");

        assert!(catalog.skills().is_empty());
        assert!(error.contains("no skills"), "{error}");
    }
}
