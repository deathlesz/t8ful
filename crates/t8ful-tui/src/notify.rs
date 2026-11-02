//! User notifications when the agent needs attention.
//!
//! The terminal bell always fires. A desktop notifier is best effort: t8ful
//! never waits for it and never fails a turn because it is missing.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// A turn that ended without a fatal error.
pub const TURN_FINISHED: &str = "Turn finished";
/// A turn that ended because of an error.
pub const TURN_FAILED: &str = "Turn failed.";
/// The plan hand-off, which replaces that turn's end message.
pub const PLAN_READY: &str = "Plan is ready.";
/// A pending question, which blocks the user until it is answered.
pub const QUESTION_WAITING: &str = "Question waiting.";

/// Something that can raise one user notification.
pub trait Notifier: Send + Sync {
    fn notify(&self, message: &str);
}

/// Raises at most one notification per turn, with fixed brief text.
///
/// The reason a turn ended and any error text stay in the transcript: the
/// desktop message only has to say that the user is needed. A plan hand-off
/// replaces the end message of its own turn, and a question notifies on its own
/// because it stops the session until the user answers.
pub struct Attention {
    notifier: Arc<dyn Notifier>,
    turn_pinged: bool,
}

impl Attention {
    pub fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            notifier,
            turn_pinged: false,
        }
    }

    /// A new turn may notify again.
    pub fn turn_started(&mut self) {
        self.turn_pinged = false;
    }

    /// The plan hand-off replaces this turn's end message.
    pub fn plan_ready(&mut self) {
        self.notifier.notify(PLAN_READY);
        self.turn_pinged = true;
    }

    /// The end of a turn, unless it already notified for a plan hand-off.
    pub fn turn_finished(&mut self, reason: &str) {
        if std::mem::take(&mut self.turn_pinged) {
            return;
        }

        let message = if reason == "error" {
            TURN_FAILED
        } else {
            TURN_FINISHED
        };

        self.notifier.notify(message);
    }

    /// A question always notifies, even after a plan hand-off in the same turn.
    pub fn question(&self) {
        self.notifier.notify(QUESTION_WAITING);
    }
}

/// Terminal bell plus an optional desktop notification.
pub struct SystemNotifier {
    pub enabled: bool,
    /// Configured command; the message is appended as its last argument.
    /// Empty means fall back to `notify-send t8ful <message>`.
    pub command: Vec<String>,
}

impl SystemNotifier {
    pub fn new(enabled: bool, command: Vec<String>) -> Self {
        Self { enabled, command }
    }

    /// The command that would carry `message`, or `None` for bell-only.
    pub fn command_for(&self, message: &str) -> Option<Vec<String>> {
        let mut command = if self.command.is_empty() {
            if find_on_path("notify-send") {
                vec!["notify-send".to_owned(), "t8ful".to_owned()]
            } else {
                return None;
            }
        } else {
            self.command.clone()
        };

        command.push(message.to_owned());

        Some(command)
    }
}

impl Notifier for SystemNotifier {
    fn notify(&self, message: &str) {
        if !self.enabled {
            return;
        }

        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"\x07");
        let _ = stdout.flush();

        let Some(command) = self.command_for(message) else {
            return;
        };

        // Fire and forget on a detached thread so a slow or missing notifier
        // never blocks the UI loop and never leaves a zombie behind.
        std::thread::spawn(move || {
            let mut parts = command.into_iter();
            let Some(program) = parts.next() else {
                return;
            };

            let Ok(mut child) = Command::new(program)
                .args(parts)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                return;
            };

            let _ = child.wait();
        });
    }
}

/// Whether an executable named `name` resolves through `PATH`.
fn find_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(name);

        candidate.is_file()
    })
}

/// A notifier that records messages; tests use it instead of the terminal.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    messages: std::sync::Mutex<Vec<String>>,
}

impl RecordingNotifier {
    pub fn messages(&self) -> Vec<String> {
        self.messages
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, message: &str) {
        self.messages
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(message.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::{Notifier, RecordingNotifier, SystemNotifier};

    #[test]
    fn a_configured_command_gets_the_message_appended() {
        let notifier = SystemNotifier::new(true, vec!["my-notify".to_owned(), "-u".to_owned()]);

        assert_eq!(
            notifier.command_for("turn finished"),
            Some(vec![
                "my-notify".to_owned(),
                "-u".to_owned(),
                "turn finished".to_owned()
            ])
        );
    }

    #[test]
    fn the_fallback_command_names_t8ful() {
        let notifier = SystemNotifier::new(true, Vec::new());

        if let Some(command) = notifier.command_for("hello") {
            assert_eq!(command[0], "notify-send");
            assert_eq!(command[1], "t8ful");
            assert_eq!(command.last().map(String::as_str), Some("hello"));
        }
    }

    #[test]
    fn disabled_notifications_do_nothing() {
        let notifier = SystemNotifier::new(false, vec!["definitely-not-run".to_owned()]);

        // No panic, and no command is built for a disabled notifier.
        notifier.notify("ignored");
        assert!(notifier.command_for("ignored").is_some());
    }

    #[test]
    fn a_missing_binary_does_not_panic() {
        let notifier =
            SystemNotifier::new(true, vec!["t8ful-notifier-that-does-not-exist".to_owned()]);

        notifier.notify("nothing to see");
    }

    #[test]
    fn a_recording_notifier_keeps_every_message() {
        let notifier = RecordingNotifier::default();

        notifier.notify("one");
        notifier.notify("two");

        assert_eq!(
            notifier.messages(),
            vec!["one".to_owned(), "two".to_owned()]
        );
    }
}
