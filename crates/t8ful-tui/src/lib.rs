//! Full-screen terminal frontend for t8ful.
//!
//! The TUI consumes the same [`t8ful_core::event::AgentEvent`] stream as the
//! headless renderers. It never calls the provider or tools directly; it forwards
//! prompts and cancellations over [`bridge::TuiCommand`] and answers structured
//! questions through [`question::TuiQuestionRouter`].

pub mod bridge;
pub mod clipboard;
pub mod commands;
pub mod edit;
mod fuzzy;
pub mod images;
mod input;
pub mod markdown;
pub mod notify;
pub mod question;
pub mod run;
pub mod spinner;
pub mod state;
pub mod terminal;
pub mod theme;
pub mod ui;

pub use bridge::{
    AuthKind, AuthSnapshot, CatalogSnapshot, ContextSnapshot, McpSnapshot, ModelRow, Secret,
    TuiBridge, TuiCommand, TuiEndpoints, bridge,
};
pub use clipboard::{ClipboardError, ClipboardImage, ClipboardImageSource, SystemClipboard};
pub use commands::{COMMANDS, CommandSpec, Completion, ValueSpec, suggestions};
pub use markdown::{RenderedLine, render as render_markdown};
pub use notify::{Notifier, RecordingNotifier, SystemNotifier};
pub use question::{PendingQuestion, TuiQuestionRouter};
pub use run::{AskOutcome, TuiConfig, TuiError, ask, run as run_tui, run_with_services};
pub use state::{AppState, Entry, Header, UsageTotals, format_age, now_seconds};
pub use terminal::TerminalGuard;
pub use theme::{Theme, ThemeName};
pub use ui::{TranscriptLayout, render};
