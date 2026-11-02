//! Read-only search over past session journals.
//!
//! Transcripts already persist as versioned JSONL, so recall is a reader rather
//! than a second store: it scans the journals on demand, keeps no index, and
//! copies bounded excerpts into the session that asked. A malformed or
//! partially written line is skipped and counted, never fatal.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use memchr::memmem::{Finder, find};
use serde::Deserialize;
use t8ful_core::message::{Message, Role};
use t8ful_core::policy::Mode;
use t8ful_tools::recall::{
    RecallHit, RecallReport, RecallRequest, RecallRole, RecallScope, SessionRecall,
};

use crate::session::{SessionId, is_prompt, title_from_text};
use crate::state::StatePaths;

/// Hard bounds on one recall scan, in newest-first order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallLimits {
    /// Newest journals examined per call.
    pub journals: usize,
    /// Bytes read from one journal.
    pub journal_bytes: u64,
    /// Bytes read across every journal in one call.
    pub scan_bytes: u64,
}

impl Default for RecallLimits {
    fn default() -> Self {
        Self::new(200, 2 * 1024 * 1024, 32 * 1024 * 1024)
    }
}

impl RecallLimits {
    pub fn new(journals: usize, journal_bytes: u64, scan_bytes: u64) -> Self {
        Self {
            journals,
            journal_bytes,
            scan_bytes,
        }
    }
}

/// Journals, ranked by message, found in the state directory.
pub struct JournalRecall {
    paths: StatePaths,
    limits: RecallLimits,
}

impl JournalRecall {
    pub fn new(paths: StatePaths, limits: RecallLimits) -> Self {
        Self { paths, limits }
    }
}

impl SessionRecall for JournalRecall {
    fn search(&self, request: &RecallRequest) -> Result<RecallReport, String> {
        let tokens = tokenize(&request.query);

        if tokens.is_empty() {
            return Err(format!(
                "the query `{}` has no searchable words",
                request.query
            ));
        }

        // One preprocessed searcher per query word, reused for every message.
        let needles = tokens.iter().map(|token| fold(token)).collect::<Vec<_>>();
        let finders = needles
            .iter()
            .map(|needle| Finder::new(needle.as_bytes()))
            .collect::<Vec<_>>();
        let project_root = canonical(&request.project_root);
        let entries = journals(&self.paths);
        let mut report = RecallReport::default();
        let mut not_scanned = entries.len().saturating_sub(self.limits.journals);
        let mut scan = 0_u64;
        let mut sessions = Vec::new();

        for (index, entry) in entries.iter().take(self.limits.journals).enumerate() {
            if scan >= self.limits.scan_bytes {
                not_scanned += entries.len().min(self.limits.journals) - index;

                break;
            }

            let budget = self.limits.journal_bytes.min(self.limits.scan_bytes - scan);

            match scan_journal(entry, budget, request, &project_root) {
                ScanOutcome::Skipped { bytes_read } => {
                    scan += bytes_read;
                    report.sessions_skipped += 1;
                }
                ScanOutcome::Scanned(journal) => {
                    scan += journal.bytes_read;
                    report.sessions_searched += 1;

                    // A journal without a prompt has nothing to resume, so it is
                    // not a usable recall result even when a reply matches.
                    if journal.title.is_none() {
                        continue;
                    }

                    let mut matches = journal
                        .messages
                        .iter()
                        .filter_map(|message| score(message, &finders))
                        .collect::<Vec<_>>();

                    if matches.is_empty() {
                        continue;
                    }

                    matches.sort_by(rank);
                    // A chatty session must not fill the whole answer, so only
                    // its two strongest messages are kept.
                    matches.truncate(2);
                    sessions.push(SessionCandidates {
                        id: entry.id.clone(),
                        journal,
                        matches,
                    });
                }
            }
        }

        // Sessions rank by their best message, so the hit that made a session
        // interesting is the first one emitted for it.
        sessions.sort_by(|left, right| {
            rank(&right.matches[0], &left.matches[0]).then_with(|| left.id.cmp(&right.id))
        });

        'sessions: for session in &sessions {
            for message in &session.matches {
                report.hits.push(RecallHit {
                    session_id: session.id.as_str().to_owned(),
                    title: session.journal.title.clone().unwrap_or_default(),
                    project_root: session.journal.project_root.clone(),
                    mode: session.journal.mode,
                    at: if message.at == 0 {
                        session.journal.created_at
                    } else {
                        message.at
                    },
                    role: message.role,
                    excerpt: excerpt(&message.text, &tokens),
                });

                if report.hits.len() >= request.limit {
                    break 'sessions;
                }
            }
        }

        report.sessions_not_scanned = not_scanned;

        Ok(report)
    }
}

/// One candidate journal, newest first by modification time.
struct JournalEntry {
    id: SessionId,
    path: PathBuf,
    modified: u64,
}

fn journals(paths: &StatePaths) -> Vec<JournalEntry> {
    let Ok(entries) = std::fs::read_dir(&paths.sessions) else {
        return Vec::new();
    };

    let mut journals = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let id = SessionId::parse(name.to_str()?).ok()?;
            let path = paths.journal_path(id.as_str());
            let metadata = std::fs::metadata(&path).ok()?;

            if !metadata.is_file() {
                return None;
            }

            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default();

            Some(JournalEntry { id, path, modified })
        })
        .collect::<Vec<_>>();

    journals.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.id.cmp(&right.id))
    });

    journals
}

/// The subset of a journal line recall needs.
///
/// Decoding stops at these fields, so a record written by a newer t8ful still
/// contributes its timestamp and message instead of failing the line.
#[derive(Deserialize)]
struct ScannedLine {
    #[serde(default)]
    at: u64,
    kind: String,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    project_root: Option<String>,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    mode: Option<Mode>,
}

enum ScanOutcome {
    Scanned(ScannedJournal),
    /// The journal was excluded, unreadable, or had no usable header.
    Skipped {
        bytes_read: u64,
    },
}

struct ScannedJournal {
    project_root: PathBuf,
    mode: Mode,
    created_at: u64,
    bytes_read: u64,
    title: Option<String>,
    messages: Vec<ScannedMessage>,
}

struct ScannedMessage {
    at: u64,
    role: RecallRole,
    text: String,
}

/// Read one journal up to `budget` bytes, keeping prompts and replies.
///
/// The header decides scope, so a journal for another project or the calling
/// session stops being read as soon as it names itself.
fn scan_journal(
    entry: &JournalEntry,
    budget: u64,
    request: &RecallRequest,
    project_root: &Path,
) -> ScanOutcome {
    let Ok(file) = File::open(&entry.path) else {
        return ScanOutcome::Skipped { bytes_read: 0 };
    };

    let mut reader = BufReader::new(file.take(budget));
    let mut line = Vec::new();
    let mut bytes_read = 0_u64;
    let mut header: Option<(PathBuf, Mode, u64)> = None;
    let mut messages = Vec::new();
    let mut title = None;

    loop {
        line.clear();

        let read = match reader.read_until(b'\n', &mut line) {
            Ok(read) => read,
            Err(_) => break,
        };

        if read == 0 {
            break;
        }

        bytes_read += read as u64;

        // A record cut by the byte budget says nothing about the rest of the
        // journal, and a crash can leave the same shape at the end of a file.
        if !line.ends_with(b"\n") {
            break;
        }

        let Ok(record) = serde_json::from_slice::<ScannedLine>(&line) else {
            continue;
        };

        match record.kind.as_str() {
            "header" => {
                if header.is_some() {
                    continue;
                }

                let Some(session_id) = record.session_id else {
                    return ScanOutcome::Skipped { bytes_read };
                };

                if session_id != entry.id.as_str() {
                    return ScanOutcome::Skipped { bytes_read };
                }

                let Some(stored) = record.project_root else {
                    return ScanOutcome::Skipped { bytes_read };
                };

                if request.current_session.as_deref() == Some(entry.id.as_str())
                    || (request.scope == RecallScope::Project
                        && canonical(Path::new(&stored)) != project_root)
                {
                    return ScanOutcome::Skipped { bytes_read };
                }

                header = Some((
                    canonical(Path::new(&stored)),
                    record.mode.unwrap_or_default(),
                    record.created_at,
                ));
            }
            "message" => {
                let Some(message) = record.message else {
                    continue;
                };

                if !matches!(message.role, Role::User | Role::Assistant) {
                    continue;
                }

                let text = message.text();

                if text.trim().is_empty() {
                    continue;
                }

                // A loaded skill is journaled as a user message, but its wrapper
                // is not something the user typed.
                if message.role == Role::User && !is_prompt(&message) {
                    continue;
                }

                if message.role == Role::User && title.is_none() {
                    title = title_from_text(&text);
                }

                messages.push(ScannedMessage {
                    at: record.at,
                    role: match message.role {
                        Role::Assistant => RecallRole::Assistant,
                        _ => RecallRole::User,
                    },
                    text,
                });
            }
            _ => {}
        }
    }

    let Some((project_root, mode, created_at)) = header else {
        return ScanOutcome::Skipped { bytes_read };
    };

    ScanOutcome::Scanned(ScannedJournal {
        project_root,
        mode,
        created_at,
        bytes_read,
        title,
        messages,
    })
}

/// One message that matched at least one query word.
struct Scored {
    at: u64,
    role: RecallRole,
    text: String,
    distinct: usize,
    occurrences: usize,
}

/// Score one message by distinct query words, then total occurrences.
fn score(message: &ScannedMessage, finders: &[Finder<'_>]) -> Option<Scored> {
    let folded = fold(&message.text);
    let mut distinct = 0;
    let mut occurrences = 0;

    for finder in finders {
        let count = occurrences_of(&folded, finder);

        if count > 0 {
            distinct += 1;
            occurrences += count;
        }
    }

    if occurrences == 0 {
        return None;
    }

    Some(Scored {
        at: message.at,
        role: message.role,
        text: message.text.clone(),
        distinct,
        occurrences,
    })
}

/// Strongest first: more query words, then more mentions, then a prompt before
/// a reply, then the newest message.
fn rank(left: &Scored, right: &Scored) -> std::cmp::Ordering {
    right
        .distinct
        .cmp(&left.distinct)
        .then_with(|| right.occurrences.cmp(&left.occurrences))
        .then_with(|| role_rank(left.role).cmp(&role_rank(right.role)))
        .then_with(|| right.at.cmp(&left.at))
}

fn role_rank(role: RecallRole) -> u8 {
    match role {
        RecallRole::User => 0,
        RecallRole::Assistant => 1,
    }
}

struct SessionCandidates {
    id: SessionId,
    journal: ScannedJournal,
    matches: Vec<Scored>,
}

/// Longest excerpt one hit may carry, in characters.
const EXCERPT_CHARS: usize = 600;

/// Lines of context one excerpt may span.
const EXCERPT_LINES: usize = 8;

/// Characters kept before the first match when an excerpt must be one window.
const EXCERPT_LEAD: usize = 200;

/// Longest query kept after tokenization.
const MAX_QUERY_TOKENS: usize = 12;

/// Shortest word worth searching for.
const MIN_TOKEN_CHARS: usize = 2;

/// Lowercase alphanumeric words, deduplicated and capped.
fn tokenize(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();

    for raw in query.split(|character: char| !character.is_alphanumeric()) {
        let token = raw.to_lowercase();

        if token.chars().count() < MIN_TOKEN_CHARS || tokens.contains(&token) {
            continue;
        }

        tokens.push(token);

        if tokens.len() == MAX_QUERY_TOKENS {
            break;
        }
    }

    tokens
}

/// One character's lowercase form, when it stays a single character.
///
/// Folding this way keeps character indices aligned with the original text,
/// which matters because the excerpt is cut by index.
fn fold_char(character: char) -> char {
    let mut lower = character.to_lowercase();

    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => character,
    }
}

/// Fold `text` for case-insensitive search.
///
/// `fold_char` yields exactly one character per input character, so positions
/// found in the folded text map back to the original text by character index.
fn fold(text: &str) -> String {
    text.chars().map(fold_char).collect()
}

fn occurrences_of(folded: &str, finder: &Finder<'_>) -> usize {
    finder.find_iter(folded.as_bytes()).count()
}

/// The byte offset of the earliest occurrence of any query word.
fn first_match(folded: &str, needles: &[String]) -> Option<usize> {
    needles
        .iter()
        .filter(|needle| !needle.is_empty())
        .filter_map(|needle| find(folded.as_bytes(), needle.as_bytes()))
        .min()
}

/// A window around the first match, with every query word marked.
///
/// The window covers at most [`EXCERPT_CHARS`] characters and [`EXCERPT_LINES`]
/// lines, is cut on character boundaries, and marks trimmed ends with `…`.
/// Whitespace is collapsed so one hit stays one line of output.
fn excerpt(text: &str, tokens: &[String]) -> String {
    let folded = fold(text);
    let needles = tokens.iter().map(|token| fold(token)).collect::<Vec<_>>();
    let Some(offset) = first_match(&folded, &needles) else {
        return collapse(text);
    };
    // Folding preserves the character count, so the folded byte offset counts
    // back to the same character index in the original text.
    let first = folded[..offset].chars().count();
    let chars = text.chars().collect::<Vec<_>>();
    let folded_chars = folded.chars().collect::<Vec<_>>();
    let needle_chars = needles
        .iter()
        .map(|needle| needle.chars().collect::<Vec<_>>())
        .collect::<Vec<_>>();

    let lines = line_ranges(&chars);
    let match_line = lines
        .iter()
        .position(|(start, end)| first >= *start && first < *end)
        .unwrap_or_else(|| lines.len().saturating_sub(1));
    let half = EXCERPT_LINES / 2;
    let mut start_line = match_line.saturating_sub(half);
    let mut end_line = (match_line + half).min(lines.len() - 1);
    let mut start = lines[start_line].0;
    let mut end = lines[end_line].1;

    while end - start > EXCERPT_CHARS && start_line < match_line {
        start_line += 1;
        start = lines[start_line].0;
    }

    while end - start > EXCERPT_CHARS && end_line > match_line {
        end_line -= 1;
        end = lines[end_line].1;
    }

    if end - start > EXCERPT_CHARS {
        start = first.saturating_sub(EXCERPT_LEAD);
        end = (start + EXCERPT_CHARS).min(chars.len());
        start = end.saturating_sub(EXCERPT_CHARS);
    }

    let mut rendered = String::new();

    if start > 0 {
        rendered.push('…');
    }

    let mut index = start;

    while index < end {
        match needle_at(&folded_chars, &needle_chars, index) {
            Some(length) => {
                rendered.push('«');
                rendered.extend(chars[index..index + length].iter());
                rendered.push('»');
                index += length;
            }
            None => {
                rendered.push(chars[index]);
                index += 1;
            }
        }
    }

    if end < chars.len() {
        rendered.push('…');
    }

    collapse(&rendered)
}

/// Character ranges of each line, including its newline.
fn line_ranges(chars: &[char]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;

    for (index, character) in chars.iter().enumerate() {
        if *character == '\n' {
            ranges.push((start, index + 1));
            start = index + 1;
        }
    }

    ranges.push((start, chars.len()));

    ranges
}

/// The length of the longest query word starting at `index`.
fn needle_at(folded: &[char], needles: &[Vec<char>], index: usize) -> Option<usize> {
    let mut longest: Option<usize> = None;

    for needle in needles {
        if needle.is_empty() || index + needle.len() > folded.len() {
            continue;
        }

        if folded[index..index + needle.len()].iter().eq(needle.iter()) {
            longest =
                Some(longest.map_or(needle.len(), |current: usize| current.max(needle.len())));
        }
    }

    longest
}

fn collapse(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut space = false;

    for character in text.chars() {
        if character.is_whitespace() {
            if !space {
                collapsed.push(' ');
                space = true;
            }
        } else {
            collapsed.push(character);
            space = false;
        }
    }

    collapsed.trim().to_owned()
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use t8ful_tools::recall::{RecallRequest, RecallRole, RecallScope, SessionRecall};

    use super::{JournalRecall, RecallLimits};
    use crate::state::StatePaths;

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::TempDir::new().expect("temp dir"),
            }
        }

        fn paths(&self) -> StatePaths {
            StatePaths {
                root: self.dir.path().join("state"),
                sessions: self.dir.path().join("state/sessions"),
                usage: self.dir.path().join("state/usage"),
            }
        }

        fn project(&self, name: &str) -> PathBuf {
            let project = self.dir.path().join(name);
            std::fs::create_dir_all(&project).expect("project dir");

            project
        }

        /// Write a journal of `(role, text)` messages and return the header
        /// line's byte length, which is the smallest budget that still finds a
        /// session header.
        fn journal(
            &self,
            paths: &StatePaths,
            id: &str,
            root: &Path,
            messages: &[(&str, &str)],
        ) -> usize {
            let header = format!(
                "{}\n",
                serde_json::json!({
                    "v": 1,
                    "at": 1_000,
                    "kind": "header",
                    "session_id": id,
                    "project_root": root.display().to_string(),
                    "created_at": 1_000,
                    "model": "vendor/model",
                    "mode": "coding",
                })
            );
            let mut body = header.clone();

            for (offset, (role, text)) in messages.iter().enumerate() {
                body.push_str(&format!(
                    "{}\n",
                    serde_json::json!({
                        "v": 1,
                        "at": 1_000 + offset as u64,
                        "kind": "message",
                        "message": {
                            "role": role,
                            "content": [{ "kind": "text", "text": text }],
                        },
                    })
                ));
            }

            self.write(paths, id, &body);

            header.len()
        }

        fn write(&self, paths: &StatePaths, id: &str, body: &str) {
            std::fs::create_dir_all(paths.session_dir(id)).expect("session dir");
            std::fs::write(paths.journal_path(id), body).expect("journal");
        }

        fn journal_len(&self, paths: &StatePaths, id: &str) -> u64 {
            std::fs::metadata(paths.journal_path(id))
                .expect("journal metadata")
                .len()
        }
    }

    fn request(root: &Path, query: &str) -> RecallRequest {
        RecallRequest {
            query: query.to_owned(),
            scope: RecallScope::Project,
            limit: 6,
            project_root: root.to_path_buf(),
            current_session: None,
        }
    }

    #[test]
    fn a_decision_in_an_assistant_reply_is_found() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");

        fixture.journal(
            &paths,
            "aaa",
            &project,
            &[
                ("user", "How should the sandbox be implemented?"),
                ("assistant", "Use Bubblewrap and mount the host read-only."),
            ],
        );

        let report = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "bubblewrap"))
            .expect("search");

        assert_eq!(report.hits.len(), 1, "{report:?}");
        assert_eq!(report.hits[0].session_id, "aaa");
        assert_eq!(report.hits[0].role, RecallRole::Assistant);
        assert_eq!(
            report.hits[0].title,
            "How should the sandbox be implemented?"
        );
        assert!(
            report.hits[0].excerpt.contains("«Bubblewrap»"),
            "{}",
            report.hits[0].excerpt
        );
        assert_eq!(report.sessions_searched, 1);
        assert_eq!(report.sessions_skipped, 0);
        assert_eq!(report.sessions_not_scanned, 0);
    }

    #[test]
    fn a_prompt_outranks_a_reply_at_equal_matches() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");

        fixture.journal(
            &paths,
            "aaa",
            &project,
            &[
                ("assistant", "The cache is a map."),
                ("user", "Why is the cache slow?"),
            ],
        );

        let report = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "cache"))
            .expect("search");

        assert_eq!(report.hits.len(), 2, "{report:?}");
        assert_eq!(report.hits[0].role, RecallRole::User);
        assert_eq!(report.hits[1].role, RecallRole::Assistant);
    }

    #[test]
    fn project_scope_excludes_another_project_and_all_includes_it() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");
        let other = fixture.project("other");

        fixture.journal(&paths, "aaa", &project, &[("user", "alpha decision")]);
        fixture.journal(&paths, "bbb", &other, &[("user", "alpha decision")]);

        let recall = JournalRecall::new(paths.clone(), RecallLimits::default());
        let scoped = recall
            .search(&request(&project, "alpha"))
            .expect("project search");

        assert_eq!(scoped.hits.len(), 1, "{scoped:?}");
        assert_eq!(scoped.hits[0].session_id, "aaa");
        assert_eq!(scoped.sessions_skipped, 1);

        let mut everywhere = request(&project, "alpha");

        everywhere.scope = RecallScope::All;

        let all = recall.search(&everywhere).expect("unscoped search");

        assert_eq!(all.hits.len(), 2, "{all:?}");
        assert_eq!(all.sessions_skipped, 0);
    }

    #[test]
    fn the_calling_session_is_skipped() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");

        fixture.journal(&paths, "aaa", &project, &[("user", "needle in a")]);
        fixture.journal(&paths, "bbb", &project, &[("user", "needle in b")]);

        let mut request = request(&project, "needle");

        request.current_session = Some("aaa".to_owned());

        let report = JournalRecall::new(paths, RecallLimits::default())
            .search(&request)
            .expect("search");

        assert_eq!(report.hits.len(), 1, "{report:?}");
        assert_eq!(report.hits[0].session_id, "bbb");
        assert_eq!(report.sessions_skipped, 1);
    }

    #[test]
    fn a_session_without_a_prompt_is_omitted() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");

        fixture.journal(
            &paths,
            "aaa",
            &project,
            &[("assistant", "a reply mentioning the needle")],
        );

        let report = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "needle"))
            .expect("search");

        assert!(report.hits.is_empty(), "{report:?}");
        assert_eq!(report.sessions_searched, 1);
    }

    #[test]
    fn a_loaded_skill_wrapper_is_not_searched_as_a_prompt() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");

        fixture.journal(
            &paths,
            "aaa",
            &project,
            &[
                ("user", "review the change"),
                (
                    "user",
                    "<skill name=\"review\">\nThe user loaded this skill explicitly; follow it \
                     where it applies.\nCheck the needle.\n</skill>",
                ),
                ("assistant", "The needle is fine."),
            ],
        );

        let report = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "needle"))
            .expect("search");

        assert_eq!(report.hits.len(), 1, "{report:?}");
        assert_eq!(report.hits[0].role, RecallRole::Assistant);
    }

    #[test]
    fn malformed_and_partial_lines_are_skipped_without_failing() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");
        let header = format!(
            "{}\n",
            serde_json::json!({
                "v": 1,
                "at": 1_000,
                "kind": "header",
                "session_id": "aaa",
                "project_root": project.display().to_string(),
                "created_at": 1_000,
                "mode": "coding",
            })
        );
        let body = format!(
            "{header}{{\"kind\":\"message\",\"message\":\n{}\n{}\n",
            serde_json::json!({
                "v": 1,
                "at": 1_001,
                "kind": "message",
                "message": {
                    "role": "user",
                    "content": [{ "kind": "text", "text": "find the needle" }],
                },
            }),
            "{\"v\":1,\"at\":1002,\"kind\":\"message\",\"message\":{\"role\":\"user\""
        );

        fixture.write(&paths, "aaa", &body);

        let report = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "needle"))
            .expect("search");

        assert_eq!(report.hits.len(), 1, "{report:?}");
        assert_eq!(report.hits[0].session_id, "aaa");
    }

    #[test]
    fn a_journal_byte_cap_stops_the_scan_inside_that_journal() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");
        let padding = "x".repeat(600);
        let header = fixture.journal(
            &paths,
            "aaa",
            &project,
            &[("user", &padding), ("assistant", "the needle is here")],
        );

        let capped = JournalRecall::new(
            paths.clone(),
            RecallLimits::new(10, header as u64, 1024 * 1024),
        )
        .search(&request(&project, "needle"))
        .expect("search");

        assert!(capped.hits.is_empty(), "{capped:?}");
        assert_eq!(capped.sessions_searched, 1);

        let full = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "needle"))
            .expect("search");

        assert_eq!(full.hits.len(), 1, "{full:?}");
    }

    #[test]
    fn the_scan_budget_stops_before_older_sessions() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");

        fixture.journal(&paths, "aaa", &project, &[("user", "the needle")]);
        fixture.journal(&paths, "bbb", &project, &[("user", "the needle")]);

        let first = fixture.journal_len(&paths, "aaa");
        let report = JournalRecall::new(paths, RecallLimits::new(10, 1024 * 1024, first))
            .search(&request(&project, "needle"))
            .expect("search");

        assert_eq!(report.hits.len(), 1, "{report:?}");
        assert_eq!(report.sessions_searched, 1);
        assert_eq!(report.sessions_not_scanned, 1);
    }

    #[test]
    fn the_journal_count_budget_stops_before_older_sessions() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");

        for id in ["aaa", "bbb", "ccc"] {
            fixture.journal(&paths, id, &project, &[("user", "the needle")]);
        }

        let report = JournalRecall::new(paths, RecallLimits::new(1, 1024 * 1024, 1024 * 1024))
            .search(&request(&project, "needle"))
            .expect("search");

        assert_eq!(report.hits.len(), 1, "{report:?}");
        assert_eq!(report.sessions_not_scanned, 2);
    }

    #[test]
    fn one_chatty_session_cannot_fill_the_answer() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");

        fixture.journal(
            &paths,
            "aaa",
            &project,
            &[
                ("user", "needle one"),
                ("assistant", "needle two"),
                ("user", "needle three"),
                ("assistant", "needle four"),
            ],
        );

        let report = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "needle"))
            .expect("search");

        assert_eq!(report.hits.len(), 2, "{report:?}");
    }

    #[test]
    fn a_long_message_yields_a_bounded_marked_excerpt() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");
        let text = format!(
            "{} needlestone needle {}",
            "before ".repeat(100),
            "after ".repeat(200)
        );

        fixture.journal(&paths, "aaa", &project, &[("user", &text)]);

        let report = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "needle"))
            .expect("search");
        let excerpt = &report.hits[0].excerpt;

        assert!(excerpt.starts_with('…'), "{excerpt}");
        assert!(excerpt.ends_with('…'), "{excerpt}");
        assert!(excerpt.contains("«needle»"), "{excerpt}");
        // The window itself is 600 characters; the two ellipses and the two
        // guillemets that mark it are added around that window.
        assert!(
            excerpt.chars().count() <= super::EXCERPT_CHARS + 8,
            "{}",
            excerpt.chars().count()
        );
    }

    #[test]
    fn a_multibyte_prefix_does_not_shift_the_excerpt() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");
        let text = "Grüße aus München. Der Cache ist langsam.";

        fixture.journal(&paths, "aaa", &project, &[("user", text)]);

        let recall = JournalRecall::new(paths, RecallLimits::default());
        let report = recall.search(&request(&project, "cache")).expect("search");

        assert_eq!(
            report.hits[0].excerpt,
            "Grüße aus München. Der «Cache» ist langsam."
        );

        // Folding is Unicode-aware, so a lowercase query finds an uppercase word.
        let accented = recall
            .search(&request(&project, "münchen"))
            .expect("search");

        assert_eq!(accented.hits.len(), 1, "{accented:?}");
        assert!(
            accented.hits[0].excerpt.contains("«München»"),
            "{}",
            accented.hits[0].excerpt
        );
    }

    #[test]
    fn a_query_without_searchable_words_is_an_error() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let project = fixture.project("project");
        let error = JournalRecall::new(paths, RecallLimits::default())
            .search(&request(&project, "a !"))
            .expect_err("no words");

        assert!(error.contains("no searchable words"), "{error}");
    }

    #[test]
    fn tokens_are_lowercased_deduplicated_and_capped() {
        assert_eq!(super::tokenize("The THE t a"), vec!["the".to_owned()]);
        assert_eq!(super::tokenize("a b c"), Vec::<String>::new());
        let distinct = (0..20)
            .map(|index| format!("word{index} "))
            .collect::<String>();

        assert_eq!(super::tokenize(&distinct).len(), 12);
    }
}
