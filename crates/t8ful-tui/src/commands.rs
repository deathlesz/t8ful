//! One table for slash-command display and completion.
//!
//! Dispatch stays in [`crate::run`] because arguments carry command-specific
//! meaning; this module only decides what to show and how to complete it.

use crate::bridge::{SessionSummary, SkillView};
use crate::fuzzy;

/// A fixed argument value a command accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueSpec {
    /// The literal argument text.
    pub value: &'static str,
    /// One-line description shown next to the value.
    pub description: &'static str,
}

/// A discoverable slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// Command text, including the leading slash.
    pub name: &'static str,
    /// Short usage line shown in help.
    pub usage: &'static str,
    /// Argument hint shown next to the name in the completion popup.
    pub hint: &'static str,
    /// One-line description shown next to the name.
    pub description: &'static str,
    /// Fixed argument values offered after the command and a space.
    pub values: &'static [ValueSpec],
}

/// Every command the help overlay and completion popup advertise.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        usage: "/help",
        hint: "",
        description: "show keys and commands",
        values: &[],
    },
    CommandSpec {
        name: "/model",
        usage: "/model [id]",
        hint: "[id]",
        description: "choose or set the model",
        values: &[],
    },
    CommandSpec {
        name: "/reasoning",
        usage: "/reasoning [level]",
        hint: "<level>",
        description: "set reasoning level",
        values: &[],
    },
    CommandSpec {
        name: "/show-reasoning",
        usage: "/show-reasoning",
        hint: "",
        description: "show or hide reasoning text",
        values: &[],
    },
    CommandSpec {
        name: "/plan",
        usage: "/plan",
        hint: "",
        description: "request plan mode",
        values: &[],
    },
    CommandSpec {
        name: "/code",
        usage: "/code",
        hint: "",
        description: "request coding mode",
        values: &[],
    },
    CommandSpec {
        name: "/skill",
        usage: "/skill [name] [prompt]",
        hint: "[name]",
        description: "load a skill's instructions",
        values: &[],
    },
    CommandSpec {
        name: "/login",
        usage: "/login [provider]",
        hint: "[provider]",
        description: "save an API key",
        values: PROVIDERS,
    },
    CommandSpec {
        name: "/logout",
        usage: "/logout [provider]",
        hint: "[provider]",
        description: "forget a saved API key",
        values: PROVIDERS,
    },
    CommandSpec {
        name: "/attach",
        usage: "/attach <path>",
        hint: "<path>",
        description: "attach an image to the next prompt",
        values: &[],
    },
    CommandSpec {
        name: "/new",
        usage: "/new",
        hint: "",
        description: "start a new session",
        values: &[],
    },
    CommandSpec {
        name: "/sessions",
        usage: "/sessions",
        hint: "",
        description: "list saved sessions",
        values: &[],
    },
    CommandSpec {
        name: "/resume",
        usage: "/resume <session-id>",
        hint: "<session-id>",
        description: "open a saved session",
        values: &[],
    },
    CommandSpec {
        name: "/process",
        usage: "/process [list|view|stop] [id]",
        hint: "[list|view|stop]",
        description: "inspect or stop background processes",
        values: PROCESS_ACTIONS,
    },
    CommandSpec {
        name: "/agents",
        usage: "/agents [id]",
        hint: "[id]",
        description: "list child agents and their reports",
        values: &[],
    },
    CommandSpec {
        name: "/tasks",
        usage: "/tasks",
        hint: "",
        description: "show the session task graph",
        values: &[],
    },
    CommandSpec {
        name: "/quit",
        usage: "/quit",
        hint: "",
        description: "leave t8ful",
        values: &[],
    },
];

/// Every level, used by tests that do not model a specific provider.
#[cfg(test)]
const DEFAULT_LEVELS: &[t8ful_core::provider::ReasoningEffort] =
    &t8ful_core::provider::ReasoningEffort::ALL;

/// One line of help for a reasoning level, shared by the popup and the picker.
pub fn reasoning_description(effort: t8ful_core::provider::ReasoningEffort) -> &'static str {
    use t8ful_core::provider::ReasoningEffort as Level;

    match effort {
        Level::Default => "let the provider decide",
        Level::Off => "no reasoning tokens",
        Level::Minimal => "slightest deliberation",
        Level::Low => "faster, less deliberation",
        Level::Medium => "balanced",
        Level::High => "slower, more deliberation",
        Level::XHigh => "very heavy deliberation",
        Level::Max => "the most deliberation the model offers",
    }
}

const PROVIDERS: &[ValueSpec] = &[
    ValueSpec {
        value: "openrouter",
        description: "OpenRouter API key",
    },
    ValueSpec {
        value: "opencode-go",
        description: "OpenCode Go API key",
    },
];

const PROCESS_ACTIONS: &[ValueSpec] = &[
    ValueSpec {
        value: "list",
        description: "list the processes started in this session",
    },
    ValueSpec {
        value: "view",
        description: "print the output collected so far",
    },
    ValueSpec {
        value: "stop",
        description: "stop a running process",
    },
];

/// One row in the completion popup: a command, a fixed argument value for one,
/// a dynamically generated value such as a saved session, or a project path
/// after `@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Index into [`COMMANDS`]; `None` for a file mention.
    command: Option<usize>,
    /// Index into the command's [`CommandSpec::values`] for a fixed argument.
    value: Option<usize>,
    label: String,
    hint: String,
    description: String,
    completed: String,
    mention: bool,
}

impl Completion {
    fn command(index: usize) -> Self {
        let spec = &COMMANDS[index];

        Self {
            command: Some(index),
            value: None,
            label: spec.name.to_owned(),
            hint: spec.hint.to_owned(),
            description: spec.description.to_owned(),
            completed: completed_command(spec),
            mention: false,
        }
    }

    fn fixed_value(index: usize, value: usize) -> Self {
        let spec = &COMMANDS[index];
        let argument = &spec.values[value];

        Self {
            command: Some(index),
            value: Some(value),
            label: format!("{} {}", spec.name, argument.value),
            hint: String::new(),
            description: argument.description.to_owned(),
            completed: format!("{} {}", spec.name, argument.value),
            mention: false,
        }
    }

    /// A reasoning level offered as the argument to `/reasoning`.
    fn reasoning(command: usize, effort: t8ful_core::provider::ReasoningEffort) -> Self {
        Self {
            command: Some(command),
            value: None,
            label: format!("/reasoning {}", effort.label()),
            hint: String::new(),
            description: reasoning_description(effort).to_owned(),
            completed: format!("/reasoning {}", effort.label()),
            mention: false,
        }
    }

    /// A saved session offered as the argument to `/resume`. The age of its
    /// newest message leads the description, so the popup says how recent it is.
    fn session(command: usize, id: &str, title: &str, last_message_at: u64, now: u64) -> Self {
        Self {
            command: Some(command),
            value: None,
            label: id.to_owned(),
            hint: String::new(),
            description: format!(
                "{} · {title}",
                crate::state::format_age(last_message_at, now)
            ),
            completed: format!("/resume {id}"),
            mention: false,
        }
    }

    /// A discovered skill offered as the argument to `/skill`.
    fn skill(command: usize, name: &str, description: &str) -> Self {
        Self {
            command: Some(command),
            value: None,
            label: name.to_owned(),
            hint: String::new(),
            description: description.to_owned(),
            completed: format!("/skill {name}"),
            mention: false,
        }
    }

    /// A project path offered after `@`; `prefix` is the composer text before
    /// the mention token.
    fn file(path: &str, prefix: &str) -> Self {
        let directory = path.ends_with('/');

        Self {
            command: None,
            value: None,
            label: path.to_owned(),
            hint: String::new(),
            description: if directory { "directory" } else { "file" }.to_owned(),
            completed: format!("{prefix}@{path}"),
            mention: true,
        }
    }

    /// The command this row belongs to, when it is a slash-command row.
    pub fn spec(&self) -> Option<&'static CommandSpec> {
        self.command.map(|index| &COMMANDS[index])
    }

    /// The argument value this row stands for, when it is not a bare command.
    pub fn value_spec(&self) -> Option<&'static ValueSpec> {
        let spec = self.spec()?;

        self.value.and_then(|index| spec.values.get(index))
    }

    /// Whether this row is an `@` file mention rather than a command row.
    pub fn is_mention(&self) -> bool {
        self.mention
    }

    /// The name column of the row.
    pub fn label(&self) -> String {
        self.label.clone()
    }

    /// The dim argument hint shown after the label, when the row is a command.
    pub fn hint(&self) -> &str {
        &self.hint
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    /// The composer text after accepting this row.
    pub fn completed_input(&self) -> String {
        self.completed.clone()
    }
}

/// Rows matching the composer contents.
///
/// A buffer starting with `/` offers slash commands and their arguments. Any
/// other buffer offers project paths when its final token starts with `@`; the
/// mention may sit anywhere in the text. Completion never mixes the two modes.
pub fn suggestions(
    input: &str,
    sessions: &[&SessionSummary],
    skills: &[SkillView],
    files: &[String],
    reasoning: &[t8ful_core::provider::ReasoningEffort],
    now: u64,
) -> Vec<Completion> {
    if input.starts_with('/') {
        return slash_suggestions(input, sessions, skills, reasoning, now);
    }

    mention_suggestions(input, files)
}

/// Slash-command and argument rows.
///
/// Completion is only offered while the composer holds a single token: the
/// input starts with `/` and has no further whitespace. A bare command prefix
/// offers commands. Once a command is followed by exactly one space, its fixed
/// argument values are offered instead, filtered by what has been typed so
/// far; `/resume` offers the resumable sessions instead of fixed values.
fn slash_suggestions(
    input: &str,
    sessions: &[&SessionSummary],
    skills: &[SkillView],
    reasoning: &[t8ful_core::provider::ReasoningEffort],
    now: u64,
) -> Vec<Completion> {
    let Some((name, argument)) = input.split_once(' ') else {
        if input.chars().any(char::is_whitespace) {
            return Vec::new();
        }

        return COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, spec)| spec.name.starts_with(input))
            .map(|(index, _)| Completion::command(index))
            .collect();
    };

    if argument.chars().any(char::is_whitespace) {
        return Vec::new();
    }

    let Some((index, spec)) = COMMANDS
        .iter()
        .enumerate()
        .find(|(_, spec)| spec.name == name)
    else {
        return Vec::new();
    };

    if spec.name == "/resume" {
        return session_rows(index, argument, sessions, now);
    }

    if spec.name == "/skill" {
        return skill_rows(index, argument, skills);
    }

    // Only the levels the selected model offers, so the popup never suggests
    // a level the provider would reject.
    if spec.name == "/reasoning" {
        return reasoning
            .iter()
            .filter(|effort| effort.label().starts_with(argument))
            .map(|effort| Completion::reasoning(index, *effort))
            .collect();
    }

    spec.values
        .iter()
        .enumerate()
        .filter(|(_, value)| value.value.starts_with(argument))
        .map(|(value, _)| Completion::fixed_value(index, value))
        .collect()
}

/// How many file rows are returned for a mention query. The popup itself shows
/// six, but leaving headroom lets arrow navigation reach more matches.
const MENTION_ROWS: usize = 50;

/// Project-path rows for an `@` token at the end of the composer.
fn mention_suggestions(input: &str, files: &[String]) -> Vec<Completion> {
    let start = input
        .rfind(char::is_whitespace)
        .map(|index| index + input[index..].chars().next().map_or(0, char::len_utf8))
        .unwrap_or(0);
    let Some(query) = input[start..].strip_prefix('@') else {
        return Vec::new();
    };
    let prefix = &input[..start];

    // An empty query lists the root; a trailing slash drills into that
    // directory. Anything else is fuzzy-matched against full paths.
    let mut matches = if query.is_empty() || query.ends_with('/') {
        direct_children(query.trim_end_matches('/'), files)
    } else {
        ranked_files(query, files)
    };

    matches.truncate(MENTION_ROWS);

    matches
        .into_iter()
        .map(|path| Completion::file(path, prefix))
        .collect()
}

/// Direct children of a directory, directories first and then by name.
fn direct_children<'a>(directory: &str, files: &'a [String]) -> Vec<&'a String> {
    let mut rows = files
        .iter()
        .filter(|path| {
            let rest = if directory.is_empty() {
                path.as_str()
            } else {
                let Some(rest) = path.strip_prefix(directory) else {
                    return false;
                };
                let Some(rest) = rest.strip_prefix('/') else {
                    return false;
                };

                rest
            };

            !rest.is_empty() && !rest.trim_end_matches('/').contains('/')
        })
        .collect::<Vec<_>>();

    rows.sort_by(|left, right| {
        right
            .ends_with('/')
            .cmp(&left.ends_with('/'))
            .then_with(|| left.cmp(right))
    });

    rows
}

/// Files fuzzy-matched against the full relative path.
fn ranked_files<'a>(query: &str, files: &'a [String]) -> Vec<&'a String> {
    let mut rows = files
        .iter()
        .filter_map(|path| {
            let score = fuzzy::best(query, [path.as_str()])?;

            Some((score, path))
        })
        .collect::<Vec<_>>();

    // Only the first `MENTION_ROWS` rows are ever shown. Selecting that prefix
    // in linear time and sorting just it keeps a keystroke cheap on a large
    // tree while producing the same order as sorting every match.
    if rows.len() > MENTION_ROWS {
        rows.select_nth_unstable_by(MENTION_ROWS - 1, compare_rows);
        rows.truncate(MENTION_ROWS);
    }

    rows.sort_by(compare_rows);

    rows.into_iter().map(|(_, path)| path).collect()
}

/// Ranking for file matches: best score, then shortest path, then path order.
fn compare_rows(
    (left_score, left): &(i32, &String),
    (right_score, right): &(i32, &String),
) -> std::cmp::Ordering {
    right_score
        .cmp(left_score)
        .then_with(|| left.len().cmp(&right.len()))
        .then_with(|| left.cmp(right))
}

/// Resumable sessions ranked by the best match of id and title, newest message
/// first when the scores tie.
fn session_rows(
    command: usize,
    query: &str,
    sessions: &[&SessionSummary],
    now: u64,
) -> Vec<Completion> {
    let mut ranked = sessions
        .iter()
        .filter_map(|session| {
            let score = fuzzy::best(query, [session.id.as_str(), session.title.as_str()])?;

            Some((score, session))
        })
        .collect::<Vec<_>>();

    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| right.last_message_at.cmp(&left.last_message_at))
    });

    ranked
        .into_iter()
        .map(|(_, session)| {
            Completion::session(
                command,
                &session.id,
                &session.title,
                session.last_message_at,
                now,
            )
        })
        .collect()
}

/// Skills ranked by the best match of name and description.
fn skill_rows(command: usize, query: &str, skills: &[SkillView]) -> Vec<Completion> {
    let mut ranked = skills
        .iter()
        .filter_map(|skill| {
            let score = fuzzy::best(query, [skill.name.as_str(), skill.description.as_str()])?;

            Some((score, skill))
        })
        .collect::<Vec<_>>();

    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.name.cmp(&right.name))
    });

    ranked
        .into_iter()
        .map(|(_, skill)| Completion::skill(command, &skill.name, &skill.description))
        .collect()
}

/// The composer text after completing a command: names with arguments get a
/// trailing space so typing, or the value popup, can continue immediately.
fn completed_command(spec: &CommandSpec) -> String {
    if spec.usage.split_whitespace().count() > 1 {
        format!("{} ", spec.name)
    } else {
        spec.name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use crate::bridge::{SessionSummary, SkillView};

    use super::COMMANDS;

    /// The reference time the completions below are rendered against.
    const NOW: u64 = 1_000_000;

    /// Command and fixed-value completion never depends on saved sessions or
    /// skills unless a test supplies them.
    fn suggestions(input: &str) -> Vec<super::Completion> {
        super::suggestions(input, &[], &[], &[], super::DEFAULT_LEVELS, NOW)
    }

    /// Borrowed session rows, as the composer passes them.
    fn refs(sessions: &[SessionSummary]) -> Vec<&SessionSummary> {
        sessions.iter().collect()
    }

    fn session(id: &str, title: &str, last_message_at: u64) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            title: title.to_owned(),
            last_message_at,
            project: "project".to_owned(),
            resumable: true,
        }
    }

    fn labels(input: &str) -> Vec<String> {
        suggestions(input)
            .into_iter()
            .map(|completion| completion.label())
            .collect()
    }

    fn names(input: &str) -> Vec<&'static str> {
        suggestions(input)
            .into_iter()
            .filter_map(|completion| completion.spec().map(|spec| spec.name))
            .collect()
    }

    #[test]
    fn a_bare_slash_lists_every_command() {
        assert_eq!(suggestions("/").len(), COMMANDS.len());
    }

    #[test]
    fn prefixes_match_only_the_first_token() {
        assert_eq!(names("/re"), vec!["/reasoning", "/resume"]);
        assert_eq!(names("hello"), Vec::<&str>::new());
        assert_eq!(names("/reasoning  high"), Vec::<&str>::new());
        assert_eq!(names("/missing"), Vec::<&str>::new());
    }

    #[test]
    fn reasoning_rows_cover_the_extended_vocabulary() {
        assert_eq!(
            labels("/reasoning "),
            vec![
                "/reasoning default",
                "/reasoning off",
                "/reasoning minimal",
                "/reasoning low",
                "/reasoning medium",
                "/reasoning high",
                "/reasoning xhigh",
                "/reasoning max",
            ]
        );
        assert_eq!(labels("/reasoning x"), vec!["/reasoning xhigh"]);
    }

    #[test]
    fn reasoning_rows_follow_the_models_own_levels() {
        use t8ful_core::provider::ReasoningEffort;

        let rows = super::suggestions(
            "/reasoning ",
            &[],
            &[],
            &[],
            &[
                ReasoningEffort::Default,
                ReasoningEffort::Low,
                ReasoningEffort::High,
            ],
            NOW,
        );

        assert_eq!(
            rows.iter()
                .map(|row| row.completed_input())
                .collect::<Vec<_>>(),
            vec!["/reasoning default", "/reasoning low", "/reasoning high"]
        );

        // A level the model does not offer is never suggested.
        assert!(
            super::suggestions(
                "/reasoning max",
                &[],
                &[],
                &[],
                &[ReasoningEffort::Default, ReasoningEffort::Low],
                NOW,
            )
            .is_empty()
        );
    }

    #[test]
    fn a_command_plus_space_offers_its_fixed_values() {
        assert_eq!(labels("/reasoning h"), vec!["/reasoning high"]);
        assert_eq!(
            labels("/login "),
            vec!["/login openrouter", "/login opencode-go"]
        );
        assert_eq!(
            labels("/login o"),
            vec!["/login openrouter", "/login opencode-go"]
        );
        assert_eq!(labels("/login x"), Vec::<String>::new());
    }

    #[test]
    fn a_command_without_values_offers_nothing_after_a_space() {
        assert!(suggestions("/plan ").is_empty());
        assert!(suggestions("/reasoning\thigh").is_empty());
        assert!(suggestions("/reasoning  high").is_empty());
    }

    #[test]
    fn accepting_a_value_inserts_the_whole_command() {
        let completion = suggestions("/reasoning h")
            .into_iter()
            .next()
            .expect("value");

        assert_eq!(completion.completed_input(), "/reasoning high");

        let login = suggestions("/login ").into_iter().next().expect("value");

        assert_eq!(login.completed_input(), "/login openrouter");
    }

    #[test]
    fn completion_adds_a_space_only_for_arguments() {
        let model = suggestions("/model")
            .into_iter()
            .find(|completion| completion.spec().is_some_and(|spec| spec.name == "/model"))
            .expect("model");
        let plan = suggestions("/plan")
            .into_iter()
            .find(|completion| completion.spec().is_some_and(|spec| spec.name == "/plan"))
            .expect("plan");

        assert_eq!(model.completed_input(), "/model ");
        assert_eq!(plan.completed_input(), "/plan");
    }

    #[test]
    fn the_command_table_advertises_the_expected_commands() {
        let command = suggestions("/re")
            .into_iter()
            .find(|completion| {
                completion
                    .spec()
                    .is_some_and(|spec| spec.name == "/reasoning")
            })
            .expect("command");

        assert_eq!(command.label(), "/reasoning");
        assert_eq!(command.hint(), "<level>");
        assert_eq!(command.description(), "set reasoning level");

        let level = suggestions("/reasoning ")
            .into_iter()
            .next()
            .expect("value");

        assert_eq!(level.hint(), "");
        assert_eq!(level.description(), "let the provider decide");

        assert_eq!(names("/log"), vec!["/login", "/logout"]);

        // `/coding` is a hidden alias; `/queue` and `/paste` were removed.
        for hidden in ["/coding", "/queue", "/paste"] {
            assert!(!COMMANDS.iter().any(|spec| spec.name == hidden), "{hidden}");
            assert!(suggestions(hidden).is_empty(), "{hidden}");
        }
    }

    #[test]
    fn resume_offers_saved_sessions_with_their_titles() {
        let sessions = vec![
            session("session-new", "fix the picker", 30),
            session("session-old", "write the docs", 10),
        ];
        let rows = super::suggestions(
            "/resume ",
            &refs(&sessions),
            &[],
            &[],
            super::DEFAULT_LEVELS,
            NOW,
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label(), "session-new");
        assert_eq!(rows[0].hint(), "");
        assert_eq!(rows[0].description(), "1w · fix the picker");
        assert_eq!(rows[0].completed_input(), "/resume session-new");
        assert_eq!(rows[0].spec().expect("command").name, "/resume");
        assert!(rows[0].value_spec().is_none());
    }

    #[test]
    fn resume_filtering_matches_ids_and_titles() {
        let sessions = vec![
            session("session-alpha", "fix the picker", 30),
            session("session-beta", "write the docs", 20),
        ];

        assert_eq!(
            super::suggestions(
                "/resume beta",
                &refs(&sessions),
                &[],
                &[],
                super::DEFAULT_LEVELS,
                NOW
            )
            .into_iter()
            .map(|row| row.label())
            .collect::<Vec<_>>(),
            vec!["session-beta"]
        );
        assert_eq!(
            super::suggestions(
                "/resume docs",
                &refs(&sessions),
                &[],
                &[],
                super::DEFAULT_LEVELS,
                NOW
            )
            .into_iter()
            .map(|row| row.label())
            .collect::<Vec<_>>(),
            vec!["session-beta"]
        );
        assert!(
            super::suggestions(
                "/resume zzz",
                &refs(&sessions),
                &[],
                &[],
                super::DEFAULT_LEVELS,
                NOW
            )
            .is_empty()
        );
    }

    fn skill(name: &str, description: &str) -> SkillView {
        SkillView {
            name: name.to_owned(),
            description: description.to_owned(),
            source: "user".to_owned(),
            commit: None,
        }
    }

    #[test]
    fn skill_offers_discovered_skills_by_name_and_description() {
        let skills = vec![
            skill("review", "Review a change for correctness."),
            skill("release", "Prepare a release."),
        ];
        let rows = super::suggestions("/skill ", &[], &skills, &[], super::DEFAULT_LEVELS, NOW);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label(), "release");
        assert_eq!(rows[0].description(), "Prepare a release.");
        assert_eq!(rows[0].completed_input(), "/skill release");
        assert_eq!(rows[0].spec().expect("command").name, "/skill");

        assert_eq!(
            super::suggestions("/skill rev", &[], &skills, &[], super::DEFAULT_LEVELS, NOW)
                .into_iter()
                .map(|row| row.label())
                .collect::<Vec<_>>(),
            vec!["review"]
        );
        assert_eq!(
            super::suggestions(
                "/skill correctness",
                &[],
                &skills,
                &[],
                super::DEFAULT_LEVELS,
                NOW,
            )
            .into_iter()
            .map(|row| row.label())
            .collect::<Vec<_>>(),
            vec!["review"]
        );
        assert!(
            super::suggestions("/skill zzz", &[], &skills, &[], super::DEFAULT_LEVELS, NOW)
                .is_empty()
        );
    }

    #[test]
    fn skill_is_advertised_with_a_hint() {
        let completion = suggestions("/ski").into_iter().next().expect("command row");

        assert_eq!(completion.label(), "/skill");
        assert_eq!(completion.hint(), "[name]");
        assert_eq!(completion.description(), "load a skill's instructions");
        assert_eq!(completion.completed_input(), "/skill ");
    }

    #[test]
    fn tab_completion_of_a_session_keeps_the_argument_form() {
        let sessions = vec![session("session-alpha", "fix the picker", 30)];
        let completion = super::suggestions(
            "/resume alp",
            &refs(&sessions),
            &[],
            &[],
            super::DEFAULT_LEVELS,
            NOW,
        )
        .into_iter()
        .next()
        .expect("session row");

        assert_eq!(completion.completed_input(), "/resume session-alpha");
    }

    /// The full ranking the top-N selection must reproduce exactly.
    fn reference_ranked<'a>(query: &str, files: &'a [String]) -> Vec<&'a String> {
        let mut rows = files
            .iter()
            .filter_map(|path| super::fuzzy::score(query, path).map(|score| (score, path)))
            .collect::<Vec<_>>();

        rows.sort_by(super::compare_rows);

        rows.into_iter()
            .take(super::MENTION_ROWS)
            .map(|(_, path)| path)
            .collect()
    }

    #[test]
    fn mention_ranking_matches_a_full_sort_on_a_large_tree() {
        let files = (0..50_000)
            .map(|index| match index % 4 {
                0 => format!("src/fuzzy_{index:05}.rs"),
                1 => format!("crates/module{}/src/main_{index:05}.rs", index % 97),
                2 => format!("docs/notes/{index:05}-matching.md"),
                _ => format!("vendor/deep/tree/path_{index:05}/file.rs"),
            })
            .collect::<Vec<_>>();

        for query in ["a", "src/", "@", "fuzzy.rs", "zzzzz"] {
            let rows = super::ranked_files(query, &files);

            assert_eq!(rows, reference_ranked(query, &files), "query {query}");
            assert!(rows.len() <= super::MENTION_ROWS, "query {query}");
        }
    }

    fn project_files() -> Vec<String> {
        vec![
            "src/".to_owned(),
            "src/main.rs".to_owned(),
            "src/tools/".to_owned(),
            "src/tools/read.rs".to_owned(),
            "tests/".to_owned(),
            "tests/agent.rs".to_owned(),
            "README.md".to_owned(),
        ]
    }

    #[test]
    fn a_mention_completes_a_path_inside_the_prompt() {
        let files = project_files();
        let rows = super::suggestions(
            "explain @src/ma",
            &[],
            &[],
            &files,
            super::DEFAULT_LEVELS,
            NOW,
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "src/main.rs");
        assert_eq!(rows[0].completed_input(), "explain @src/main.rs");
        assert!(rows[0].is_mention());
        assert!(rows[0].spec().is_none());
    }

    #[test]
    fn a_bare_at_lists_root_entries_and_a_slash_drills_down() {
        let files = project_files();
        let root = super::suggestions("@", &[], &[], &files, super::DEFAULT_LEVELS, NOW)
            .into_iter()
            .map(|row| row.label())
            .collect::<Vec<_>>();

        assert_eq!(root, vec!["src/", "tests/", "README.md"]);

        let children = super::suggestions("@src/", &[], &[], &files, super::DEFAULT_LEVELS, NOW)
            .into_iter()
            .map(|row| row.label())
            .collect::<Vec<_>>();

        assert_eq!(children, vec!["src/tools/", "src/main.rs"]);
    }

    #[test]
    fn mention_rows_rank_fuzzy_paths_and_enforce_a_limit() {
        let files = vec!["src/read.rs".to_owned(), "src/reader.rs".to_owned()];
        let rows = super::suggestions("@reader", &[], &[], &files, super::DEFAULT_LEVELS, NOW);

        assert_eq!(rows[0].label(), "src/reader.rs");
        assert_eq!(rows.len(), 1, "{rows:?}");

        let many = (0..80)
            .map(|index| format!("file-{index}.rs"))
            .collect::<Vec<_>>();
        let rows = super::suggestions("@file", &[], &[], &many, super::DEFAULT_LEVELS, NOW);

        assert_eq!(rows.len(), super::MENTION_ROWS);
    }

    #[test]
    fn mentions_only_match_the_final_token_and_never_slash_commands() {
        let files = project_files();

        assert!(
            super::suggestions("mail foo@bar", &[], &[], &files, super::DEFAULT_LEVELS, NOW)
                .is_empty()
        );
        assert!(
            super::suggestions(
                "@src/main.rs done",
                &[],
                &[],
                &files,
                super::DEFAULT_LEVELS,
                NOW
            )
            .is_empty()
        );
        assert!(
            super::suggestions("/attach @src", &[], &[], &files, super::DEFAULT_LEVELS, NOW)
                .is_empty()
        );
    }
}
