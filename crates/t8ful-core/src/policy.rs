//! Operation mode and the central dispatch gate.
//!
//! Coding mode is fully autonomous: every tool call is allowed. Plan mode
//! denies file mutation, but direct reads are not restricted; shell commands
//! run in a sandbox whose mounts enforce the plan boundary.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::tool::ToolEffect;

/// Operation mode. Only explicit user input can change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Coding,
    Plan,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Coding => "coding",
            Self::Plan => "plan",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "coding" => Some(Self::Coding),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }

    /// The stricter of two modes: plan is a ceiling, never a grant.
    ///
    /// Resume uses this to restore a journaled mode without letting it widen a
    /// plan mode configured for the current invocation.
    pub fn most_restrictive(self, other: Self) -> Self {
        if self == Self::Plan || other == Self::Plan {
            Self::Plan
        } else {
            Self::Coding
        }
    }
}

/// The `<mode_change>` section for the first request after a real switch.
///
/// It names the transition and what it means for the turn ahead, so the model
/// never has to infer the live mode from an older environment block. Startup
/// and resume have no earlier mode in the conversation and never call this.
pub fn mode_change_section(previous: Mode, mode: Mode) -> Option<String> {
    let consequence = match (previous, mode) {
        (Mode::Coding, Mode::Plan) => {
            "Plan-mode restrictions apply from this request onward: do not modify project files \
now, even if you were editing them before the user switched."
        }
        (Mode::Plan, Mode::Coding) => {
            "Plan-mode restrictions no longer apply, so writing is allowed again. Any plan \
presented earlier is a proposal; the user's message is the instruction."
        }
        _ => return None,
    };

    Some(format!(
        "<mode_change>\nThe user switched this session from {} mode to {} mode.\n\
{consequence}\n</mode_change>",
        previous.label(),
        mode.label(),
    ))
}

/// Outcome of the central policy gate for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny { reason: String },
}

/// Scratch locations that never belong to the project.
///
/// Plan mode gives each sandboxed command a private writable tmpfs here rather
/// than mounting the host directory.
pub fn scratch_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];
    let temp = std::env::temp_dir();

    if !paths.contains(&temp) {
        paths.push(temp);
    }

    paths
}

/// Well-known package and tool caches under a home directory.
pub fn package_cache_paths_under(home: &std::path::Path) -> Vec<PathBuf> {
    const RELATIVE: &[&str] = &[
        ".cargo/registry",
        ".cargo/git",
        ".rustup",
        ".npm",
        ".cache/pip",
        ".cache/uv",
        ".cache/go-build",
        "go/pkg/mod",
        ".gradle",
        ".m2",
        ".pub-cache",
        ".cache/zig",
        ".local/share/uv",
        ".local/state",
    ];
    let mut paths = vec![PathBuf::from("/nix/store")];

    paths.extend(RELATIVE.iter().map(|relative| home.join(relative)));
    paths
}

/// Package and tool caches that plan mode mounts read-only.
pub fn package_cache_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return vec![PathBuf::from("/nix/store")];
    };

    package_cache_paths_under(&home)
}

/// Central dispatch gate. Every dispatch path, including subagents and MCP
/// tools, must go through [`Policy::evaluate`].
///
/// Coding mode has no guardrails: writes outside the project and dangerous
/// shell commands run. Plan mode denies the mutating tool effect; the shell
/// boundary is enforced by the sandbox mounts rather than this gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    mode: Mode,
}

impl Policy {
    pub fn new(mode: Mode) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn evaluate(&self, effect: ToolEffect) -> Decision {
        if self.mode == Mode::Plan && effect == ToolEffect::Mutating {
            return Decision::Deny {
                reason: "plan mode does not allow file modification; present a plan and leave \
                         plan mode before changing files"
                    .to_owned(),
            };
        }

        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Decision, Mode, Policy};
    use crate::tool::ToolEffect;

    #[test]
    fn coding_mode_allows_every_effect() {
        let policy = Policy::new(Mode::Coding);

        for effect in [
            ToolEffect::ReadOnly,
            ToolEffect::Mutating,
            ToolEffect::Process,
            ToolEffect::Control,
        ] {
            assert_eq!(policy.evaluate(effect), Decision::Allow, "{effect:?}");
        }
    }

    #[test]
    fn plan_mode_denies_only_mutation() {
        let policy = Policy::new(Mode::Plan);

        assert!(matches!(
            policy.evaluate(ToolEffect::Mutating),
            Decision::Deny { .. }
        ));

        for effect in [
            ToolEffect::ReadOnly,
            ToolEffect::Process,
            ToolEffect::Control,
        ] {
            assert_eq!(policy.evaluate(effect), Decision::Allow, "{effect:?}");
        }
    }

    #[test]
    fn changing_mode_changes_the_gate() {
        let mut policy = Policy::new(Mode::Coding);

        assert_eq!(policy.evaluate(ToolEffect::Mutating), Decision::Allow);

        policy.set_mode(Mode::Plan);

        assert!(matches!(
            policy.evaluate(ToolEffect::Mutating),
            Decision::Deny { .. }
        ));

        policy.set_mode(Mode::Coding);

        assert_eq!(policy.evaluate(ToolEffect::Mutating), Decision::Allow);
    }

    #[test]
    fn a_mode_change_section_names_both_modes_and_the_consequence() {
        let plan = super::mode_change_section(Mode::Coding, Mode::Plan).expect("coding to plan");

        assert!(plan.starts_with("<mode_change>\n"), "{plan}");
        assert!(plan.contains("from coding mode to plan mode"), "{plan}");
        assert!(plan.contains("do not modify project files now"), "{plan}");
        assert!(plan.ends_with("</mode_change>"), "{plan}");

        let coding = super::mode_change_section(Mode::Plan, Mode::Coding).expect("plan to coding");

        assert!(coding.contains("from plan mode to coding mode"), "{coding}");
        assert!(coding.contains("restrictions no longer apply"), "{coding}");
        assert!(coding.contains("proposal"), "{coding}");
    }

    #[test]
    fn the_same_mode_produces_no_section() {
        assert!(super::mode_change_section(Mode::Plan, Mode::Plan).is_none());
        assert!(super::mode_change_section(Mode::Coding, Mode::Coding).is_none());
    }

    #[test]
    fn the_most_restrictive_mode_is_plan_whenever_either_side_is_plan() {
        for (left, right, expected) in [
            (Mode::Coding, Mode::Coding, Mode::Coding),
            (Mode::Coding, Mode::Plan, Mode::Plan),
            (Mode::Plan, Mode::Coding, Mode::Plan),
            (Mode::Plan, Mode::Plan, Mode::Plan),
        ] {
            assert_eq!(left.most_restrictive(right), expected, "{left:?}+{right:?}");
        }
    }

    #[test]
    fn scratch_and_cache_paths_are_known() {
        let scratch = super::scratch_paths();

        assert!(scratch.contains(&PathBuf::from("/tmp")));
        assert!(scratch.contains(&PathBuf::from("/var/tmp")));

        let dir = tempfile::tempdir().expect("temp home");
        let roots = super::package_cache_paths_under(dir.path());

        assert!(roots.contains(&PathBuf::from("/nix/store")));
        assert!(roots.contains(&dir.path().join(".cargo/registry")));
    }
}
