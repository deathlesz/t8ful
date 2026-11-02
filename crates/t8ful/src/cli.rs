//! Command-line surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "t8ful",
    version,
    about = "A Rust coding agent. Thoughts matter.",
    long_about = "t8ful is a coding agent for the terminal. Run `t8ful` for the full-screen interface, \
                  or `t8ful run` for headless execution that streams text or versioned JSONL events."
)]
pub struct Cli {
    /// Configuration file that overrides project, user, and system settings.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Log level for diagnostics (error, warn, info, debug, trace). The
    /// full-screen interface writes them to `$XDG_STATE_HOME/t8ful/logs/t8ful.log`
    /// so they cannot overwrite the dashboard; other commands use stderr.
    #[arg(long, global = true, value_name = "LEVEL", env = "T8FUL_LOG")]
    pub log: Option<String>,

    /// Resume an existing session by id. New invocations start a new session.
    #[arg(long, global = true, value_name = "ID")]
    pub resume: Option<String>,

    /// Operation mode. Only explicit user input can change it.
    #[arg(long, global = true, value_enum, value_name = "MODE")]
    pub mode: Option<ModeArg>,

    /// Trust this project's configuration for this run only.
    ///
    /// Project configuration may define MCP servers and select the provider
    /// endpoint. It is restricted unless the project has been trusted, which is
    /// normally decided interactively. This flag grants that trust without
    /// recording it, so it is safe in a script.
    #[arg(long, global = true)]
    pub trust_project: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one prompt without the full-screen interface.
    Run(RunArgs),

    /// List the models the configured account can use.
    Models(ModelsArgs),

    /// List saved sessions without contacting the provider.
    Sessions(SessionsArgs),

    /// Show historical token and cost totals without contacting the provider.
    Usage(UsageArgs),

    /// List, install, or update skills without contacting the provider.
    Skills(SkillsArgs),

    /// Inspect or revoke per-project trust decisions.
    Trust(TrustArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Prompt text. Use `-` to read the prompt from stdin; stdin is also read
    /// when no prompt is given.
    pub prompt: Option<String>,

    /// Output format written to stdout.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// OpenRouter model id for this run.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Maximum model steps in the turn. Unlimited when omitted.
    #[arg(long, value_name = "N")]
    pub max_steps: Option<u32>,

    /// Attach an image to the prompt. The file is copied into session storage.
    #[arg(long, value_name = "PATH")]
    pub attach: Vec<PathBuf>,

    /// Load a skill before the prompt. Repeatable and applied in order.
    #[arg(long, value_name = "NAME")]
    pub skill: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ModelsArgs {
    /// Emit a JSON array instead of one model per line.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionsArgs {
    /// Emit a JSON array instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct UsageArgs {
    /// Emit JSON instead of a readable report.
    #[arg(long)]
    pub json: bool,

    /// Rebuild totals from session journals before printing.
    #[arg(long)]
    pub rebuild: bool,
}

#[derive(Debug, Args)]
pub struct TrustArgs {
    #[command(subcommand)]
    pub command: TrustCommand,
}

#[derive(Debug, Subcommand)]
pub enum TrustCommand {
    /// List trusted projects.
    List,

    /// Revoke the trust decision for a project.
    Revoke(TrustRevokeArgs),
}

#[derive(Debug, Args)]
pub struct TrustRevokeArgs {
    /// Project root to distrust. Defaults to the current project.
    pub path: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SkillsArgs {
    #[command(subcommand)]
    pub command: SkillsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    /// List discovered skills.
    List(SkillsListArgs),

    /// Install a skill from a Git repository.
    Install(SkillsInstallArgs),

    /// Update a skill that t8ful installed.
    Update(SkillsUpdateArgs),
}

#[derive(Debug, Args)]
pub struct SkillsListArgs {
    /// Emit a JSON array instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SkillsInstallArgs {
    /// Git repository URL or path.
    pub source: String,

    /// Revision to check out; the resolved commit is recorded either way.
    #[arg(long, value_name = "REV")]
    pub rev: Option<String>,

    /// Subdirectory inside the repository that holds the skill.
    #[arg(long, value_name = "PATH")]
    pub subdir: Option<String>,

    /// Directory name for the installed skill.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Install under <project-root>/.t8ful/skills instead of the user directory.
    #[arg(long)]
    pub project: bool,
}

#[derive(Debug, Args)]
pub struct SkillsUpdateArgs {
    /// Name of a discovered skill that t8ful installed.
    pub name: String,

    /// Move to this revision instead of the recorded one or the default branch.
    #[arg(long, value_name = "REV")]
    pub rev: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    Coding,
    Plan,
}

impl From<ModeArg> for t8ful_core::policy::Mode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Coding => Self::Coding,
            ModeArg::Plan => Self::Plan,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable streaming text.
    Text,

    /// One versioned JSON event per line.
    Json,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, OutputFormat};

    #[test]
    fn bare_invocation_launches_the_tui() {
        let cli = Cli::try_parse_from(["t8ful"]).expect("parses");

        assert!(cli.command.is_none());
    }

    #[test]
    fn run_defaults_to_text_output() {
        let cli = Cli::try_parse_from(["t8ful", "run", "hello"]).expect("parses");

        let Some(Command::Run(args)) = cli.command else {
            panic!("expected the run subcommand");
        };

        assert_eq!(args.format, OutputFormat::Text);
        assert_eq!(args.prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn run_accepts_json_and_run_options() {
        let cli = Cli::try_parse_from([
            "t8ful",
            "run",
            "go",
            "--format",
            "json",
            "--model",
            "vendor/model",
            "--max-steps",
            "4",
        ])
        .expect("parses");

        let Some(Command::Run(args)) = cli.command else {
            panic!("expected the run subcommand");
        };

        assert_eq!(args.format, OutputFormat::Json);
        assert_eq!(args.model.as_deref(), Some("vendor/model"));
        assert_eq!(args.max_steps, Some(4));
    }

    #[test]
    fn removed_authorization_flags_are_rejected() {
        for flag in ["--allow-dangerous", "--autonomous"] {
            let error = Cli::try_parse_from(["t8ful", "run", "go", flag]).expect_err("rejected");

            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{flag}"
            );
        }
    }

    #[test]
    fn models_defaults_to_plain_output() {
        let cli = Cli::try_parse_from(["t8ful", "models"]).expect("parses");

        let Some(Command::Models(args)) = cli.command else {
            panic!("expected the models subcommand");
        };

        assert!(!args.json);
    }

    #[test]
    fn unknown_subcommands_are_rejected() {
        let error = Cli::try_parse_from(["t8ful", "nonsense"]).expect_err("rejected");

        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn run_accepts_repeatable_skill_activations() {
        let cli = Cli::try_parse_from([
            "t8ful", "run", "go", "--skill", "review", "--skill", "release",
        ])
        .expect("parses");

        let Some(Command::Run(args)) = cli.command else {
            panic!("expected the run subcommand");
        };

        assert_eq!(args.skill, vec!["review", "release"]);
    }

    #[test]
    fn the_skills_subcommands_parse_their_options() {
        assert!(matches!(
            Cli::try_parse_from(["t8ful", "skills", "list", "--json"])
                .unwrap()
                .command,
            Some(Command::Skills(super::SkillsArgs {
                command: super::SkillsCommand::List(super::SkillsListArgs { json: true })
            }))
        ));

        let Some(Command::Skills(super::SkillsArgs {
            command: super::SkillsCommand::Install(args),
        })) = Cli::try_parse_from([
            "t8ful",
            "skills",
            "install",
            "https://example.com/review.git",
            "--rev",
            "v1",
            "--subdir",
            "skills/review",
            "--name",
            "review",
            "--project",
        ])
        .unwrap()
        .command
        else {
            panic!("expected skills install");
        };

        assert_eq!(args.source, "https://example.com/review.git");
        assert_eq!(args.rev.as_deref(), Some("v1"));
        assert_eq!(args.subdir.as_deref(), Some("skills/review"));
        assert_eq!(args.name.as_deref(), Some("review"));
        assert!(args.project);

        assert!(matches!(
            Cli::try_parse_from(["t8ful", "skills", "update", "review", "--rev", "main"])
                .unwrap()
                .command,
            Some(Command::Skills(super::SkillsArgs {
                command: super::SkillsCommand::Update(_)
            }))
        ));
    }
}

#[cfg(test)]
mod phase_two_tests {
    use clap::Parser;

    use super::{Cli, Command, ModeArg};

    #[test]
    fn resume_and_mode_are_global_options() {
        let cli = Cli::try_parse_from([
            "t8ful",
            "--resume",
            "session-1",
            "--mode",
            "plan",
            "run",
            "hello",
        ])
        .expect("parses");

        assert_eq!(cli.resume.as_deref(), Some("session-1"));
        assert_eq!(cli.mode, Some(ModeArg::Plan));
        assert!(matches!(cli.command, Some(Command::Run(_))));
    }

    #[test]
    fn sessions_and_usage_are_offline_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["t8ful", "sessions", "--json"])
                .unwrap()
                .command,
            Some(Command::Sessions(_))
        ));
        assert!(matches!(
            Cli::try_parse_from(["t8ful", "usage", "--rebuild"])
                .unwrap()
                .command,
            Some(Command::Usage(_))
        ));
    }
}
