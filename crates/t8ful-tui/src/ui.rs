//! Rendering. Everything here is a pure function of [`AppState`] so it can be
//! exercised with ratatui's `TestBackend`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use t8ful_core::policy::Mode;
use t8ful_core::question::Question;

use crate::commands::COMMANDS;
use crate::images::{ImagePreviews, PreviewKey, PreviewOutcome};
use crate::markdown::StreamRow;
use crate::state::{
    AppState, Entry, PlanChoice, Point, PreviewKind, QuestionProgress, ToolPreview,
    friendly_tool_name,
};
use crate::theme::Theme;

pub fn render(frame: &mut Frame, state: &mut AppState, layout: &mut TranscriptLayout) {
    let area = frame.area();

    if let Some(bg) = state.theme.bg {
        frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
    }

    let notice = queue_notice(state, area.width as usize);
    let todo_lines = todo_block(state, area.width as usize, area.height);
    let follow_ups = follow_up_lines(state, area.width as usize);
    let attachments = attachment_tray_lines(state, area.width as usize);
    let menu_open = state.question.is_some() || state.plan_choice.is_some();
    let completion_height = if menu_open {
        0
    } else {
        completion_popup_height(state)
    };
    // The composer is wrapped and its cursor located once per frame; the input
    // widget draws the lines this pass already produced.
    let composer = composer_layout(
        state.input(),
        state.input_editor().cursor(),
        area.width as usize,
    );
    let input_height = if menu_open { 0 } else { composer.height() };
    let footer_height = area.height.clamp(1, 2);
    let notice_height = u16::from(notice.is_some());
    let todo_height = todo_lines.len() as u16;
    let follow_up_height = follow_ups.len() as u16;
    let attachment_height = attachments.len() as u16;
    // Keep one transcript row, both rules, and the footer; the panel takes the
    // rest of the composer slot and grows into the transcript when it needs to.
    let reserved = 1_u16
        .saturating_add(1)
        .saturating_add(notice_height)
        .saturating_add(todo_height)
        .saturating_add(follow_up_height)
        .saturating_add(attachment_height)
        .saturating_add(1)
        .saturating_add(footer_height);
    let panel_max = area.height.saturating_sub(reserved).max(1) as usize;
    let menu = if let Some(progress) = &state.question {
        Some(question_panel(
            progress,
            &state.theme,
            area.width,
            panel_max,
        ))
    } else {
        state
            .plan_choice
            .as_ref()
            .map(|choice| plan_panel(choice, &state.theme, area.width, panel_max))
    };
    let panel_height = menu.as_ref().map(MenuPanel::height).unwrap_or(0);
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(notice_height),
        Constraint::Length(todo_height),
        Constraint::Length(follow_up_height),
        Constraint::Length(attachment_height),
        Constraint::Length(panel_height),
        Constraint::Length(completion_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
        Constraint::Length(footer_height),
    ])
    .split(area);

    render_transcript(frame, chunks[0], state, layout);
    render_rule(frame, chunks[1], state);

    if let Some(notice) = notice {
        render_notice(frame, chunks[2], state, &notice);
    }

    if !todo_lines.is_empty() {
        render_todos(frame, chunks[3], &todo_lines);
    }

    if !follow_ups.is_empty() {
        render_follow_ups(frame, chunks[4], state, &follow_ups);
    }

    if !attachments.is_empty() {
        render_attachment_tray(frame, chunks[5], &attachments);
    }

    if let Some(menu) = &menu {
        render_menu_panel(frame, chunks[6], &state.theme, menu);
    }

    render_completions(frame, chunks[7], state);
    render_input(frame, chunks[8], &composer);
    render_rule(frame, chunks[9], state);
    render_footer(frame, chunks[10], state);

    if let Some(selected) = state.picker {
        render_picker(frame, area, state, selected);
    }

    if let Some(selected) = state.session_picker {
        render_session_picker(frame, area, state, selected);
    }

    if let Some(selected) = state.reasoning_picker {
        render_reasoning_picker(frame, area, state, selected);
    }

    if state.help {
        render_help(frame, area, state);
    }

    if let Some(login) = &state.login {
        render_login(frame, area, state, login);
    }
}

fn panel_style(theme: &Theme) -> Style {
    match theme.bg {
        Some(bg) => Style::default().bg(bg),
        None => Style::default(),
    }
}

fn render_help(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let width = area.width.saturating_sub(8).clamp(20, 78).min(area.width);
    // Every row is useful, so the overlay takes what it needs and only then
    // gives up space to the terminal.
    let height = area.height.clamp(8, 48);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let heading = Style::default()
        .fg(theme.title)
        .add_modifier(Modifier::BOLD);
    let hint = Style::default().fg(theme.dim);
    let text = Style::default().fg(theme.fg);
    let mode = state.header.mode.label();
    let location_width = inner.width as usize;
    let (model, model_style) = if state.header.model.trim().is_empty() {
        ("no model".to_owned(), Style::default().fg(theme.dim))
    } else {
        (state.header.model.clone(), Style::default().fg(theme.fg))
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("t8ful v{}", state.header.version),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" · {model} · mode: {mode}"), model_style),
        ]),
        Line::from(Span::styled("location", heading)),
        Line::from(Span::styled(
            elide(
                &format!("session {}", state.header.session_id),
                location_width,
            ),
            hint,
        )),
        Line::from(Span::styled(
            elide(
                &format!("root    {}", display_path(&state.header.project_root)),
                location_width,
            ),
            hint,
        )),
        Line::from(Span::styled(
            elide(
                &format!("cwd     {}", display_path(&state.header.launch_dir)),
                location_width,
            ),
            hint,
        )),
        Line::from(Span::styled("keys", heading)),
        Line::from(Span::styled(
            "Enter send or steer · Esc cancel, clear, or close",
            text,
        )),
        Line::from(Span::styled(
            "Ctrl-C cancel or quit · Ctrl-D quit · Ctrl-Q queue follow-up",
            text,
        )),
        Line::from(Span::styled(
            "Ctrl-V paste image · Ctrl-O expand tool output · Ctrl-R reasoning text",
            text,
        )),
        Line::from(Span::styled(
            "Alt-E reasoning level · Tab plan/code · PgUp/PgDn scroll",
            text,
        )),
        Line::from(Span::styled(
            "Ctrl-A/E line start/end · Ctrl-K/U kill · Ctrl-Y yank · Alt-B/F word",
            text,
        )),
        Line::from(Span::styled(
            "Drag to select and copy · Esc clears the selection",
            text,
        )),
        Line::from(Span::styled(
            "Type / for commands · ↑/↓ choose · Tab complete · Enter run",
            text,
        )),
        Line::from(Span::styled(
            "Questions: ↑/↓ move, Enter select, 1-4 select, ←→ switch, Esc back",
            text,
        )),
        Line::from(Span::styled("commands", heading)),
    ];

    for spec in COMMANDS {
        lines.push(Line::from(vec![
            Span::styled(spec.usage, text),
            Span::styled(format!("  {}", spec.description), hint),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "? or Esc closes this overlay",
        hint,
    )));

    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(Block::default().style(panel_style(theme)), popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::NONE))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_picker(frame: &mut Frame, area: Rect, state: &AppState, selected: usize) {
    let theme = &state.theme;
    let rows = state.picker_rows();
    let body = rows.len().max(1) as u16;
    let width = area.width.saturating_sub(8).clamp(24, 60);
    let height = (body + 7).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let filter = state.picker_filter();
    let mut lines = vec![
        Line::from(Span::styled(
            "model picker",
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            if filter.is_empty() {
                "filter: all models".to_owned()
            } else {
                format!("filter: {filter}")
            },
            Style::default().fg(theme.dim),
        )),
        Line::from(""),
    ];

    if rows.is_empty() {
        let placeholder = if state.models.is_empty() && state.catalog_pending() {
            "loading models…"
        } else if state.models.is_empty() {
            "no models discovered"
        } else {
            "no matching models"
        };

        lines.push(Line::from(Span::styled(
            placeholder,
            Style::default().fg(theme.dim),
        )));
    }

    for (position, index) in rows.iter().enumerate() {
        let model = &state.models[*index];
        let chosen = position == selected;
        let marker = if chosen { "> " } else { "  " };
        let style = if chosen {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };

        lines.push(Line::from(Span::styled(format!("{marker}{model}"), style)));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "type to filter · ↑↓ choose · Enter apply at the next turn boundary · Esc close",
        Style::default().fg(theme.dim),
    )));

    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(Block::default().style(panel_style(theme)), popup);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_session_picker(frame: &mut Frame, area: Rect, state: &AppState, selected: usize) {
    let theme = &state.theme;
    let rows = state.session_picker_rows();
    let body = rows.len().max(1) as u16;
    let width = area.width.saturating_sub(8).clamp(24, 60);
    let height = (body + 7).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let filter = state.session_picker_filter();
    let mut lines = vec![
        Line::from(Span::styled(
            "resume session",
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            if filter.is_empty() {
                "filter: all sessions".to_owned()
            } else {
                format!("filter: {filter}")
            },
            Style::default().fg(theme.dim),
        )),
        Line::from(""),
    ];

    if rows.is_empty() {
        let placeholder = if !state.sessions_loaded() {
            "loading sessions…"
        } else if state.resumable_sessions().next().is_none() {
            "no saved sessions"
        } else {
            "no matching sessions"
        };

        lines.push(Line::from(Span::styled(
            placeholder,
            Style::default().fg(theme.dim),
        )));
    }

    // Every row ends with the age of its newest message, right aligned and all
    // in the same column, so the list can be read down the side.
    let now = crate::state::now_seconds();
    let ages = rows
        .iter()
        .map(|session| crate::state::format_age(session.last_message_at, now))
        .collect::<Vec<_>>();
    let age_width = ages
        .iter()
        .map(|age| age.chars().count())
        .max()
        .unwrap_or_default();

    for (position, (session, age)) in rows.iter().zip(ages.iter()).enumerate() {
        let chosen = position == selected;
        let marker = if chosen { "> " } else { "  " };
        let id_style = if chosen {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let title_width = (inner.width as usize)
            .saturating_sub(2)
            .saturating_sub(session.id.chars().count())
            .saturating_sub(2)
            .saturating_sub(age_width + 2);

        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.dim)),
            Span::styled(session.id.clone(), id_style),
            Span::raw("  "),
            Span::styled(
                elide(&session.title, title_width),
                Style::default().fg(theme.dim),
            ),
            Span::raw("  "),
            Span::styled(format!("{age:>age_width$}"), Style::default().fg(theme.dim)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "type to filter · ↑↓ choose · Enter resume · Esc close",
        Style::default().fg(theme.dim),
    )));

    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(Block::default().style(panel_style(theme)), popup);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_reasoning_picker(frame: &mut Frame, area: Rect, state: &AppState, selected: usize) {
    let theme = &state.theme;
    let width = area.width.saturating_sub(8).clamp(20, 58).min(area.width);
    let height = area.height.saturating_sub(4).clamp(8, 20).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            "reasoning level",
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    // Only the levels this model offers, so the picker never advertises a
    // choice the provider would reject.
    for (index, effort) in state.reasoning_options().iter().enumerate() {
        let chosen = index == selected;
        let marker = if chosen { "> " } else { "  " };
        let style = if chosen {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let note = if effort == &state.reasoning_effort {
            "  · current"
        } else {
            ""
        };

        lines.push(Line::from(Span::styled(
            format!("{marker}{}{note}", effort.label()),
            style,
        )));
    }

    if state.reasoning_options().len() <= 2 && state.supports_reasoning == Some(false) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "this model does not advertise reasoning",
            Style::default().fg(theme.warning),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓ choose · Enter apply · Esc close",
        Style::default().fg(theme.dim),
    )));

    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(Block::default().style(panel_style(theme)), popup);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A panel drawn in the composer slot. The question form and the plan hand-off
/// share it so their shape and hints cannot drift apart.
struct MenuPanel {
    body: Vec<Line<'static>>,
    hint: Line<'static>,
    /// Row inside `body` and column inside the panel that hold the cursor.
    cursor: Option<(usize, usize)>,
}

impl MenuPanel {
    /// Cut the body down to the rows the layout can give it, keeping the hint
    /// line and marking the truncation.
    fn clipped(mut self, max_rows: usize, theme: &Theme) -> Self {
        let max_rows = max_rows.max(1);

        if self.body.len() + 1 > max_rows {
            if max_rows == 1 {
                self.body.clear();
            } else {
                self.body.truncate(max_rows - 2);
                self.body.push(Line::from(Span::styled(
                    "…".to_owned(),
                    Style::default().fg(theme.dim),
                )));
            }

            if self.cursor.is_some_and(|(row, _)| row >= self.body.len()) {
                self.cursor = None;
            }
        }

        self
    }

    fn height(&self) -> u16 {
        (self.body.len() + 1) as u16
    }
}

fn render_menu_panel(frame: &mut Frame, area: Rect, theme: &Theme, panel: &MenuPanel) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    frame.render_widget(
        Block::default().style(Style::default().bg(theme.overlay)),
        area,
    );

    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: area.height,
    };
    let mut lines = panel.body.clone();

    lines.push(panel.hint.clone());
    frame.render_widget(Paragraph::new(lines), inner);

    if let Some((row, column)) = panel.cursor
        && row < panel.body.len().min(area.height as usize)
    {
        frame.set_cursor_position(Position::new(
            inner.x + (column as u16).min(inner.width.saturating_sub(1)),
            area.y + row as u16,
        ));
    }
}

/// One selectable row of a panel.
struct OptionRow<'a> {
    number: Option<usize>,
    /// `Some(checked)` renders a multi-select box.
    checked: Option<bool>,
    label: &'a str,
    description: &'a str,
    selected: bool,
}

fn push_option(body: &mut Vec<Line<'static>>, theme: &Theme, row: OptionRow<'_>, width: usize) {
    let width = width.max(1);
    let marker = if row.selected { "→ " } else { "  " };
    let marker_style = if row.selected {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.dim)
    };
    let dim = Style::default().fg(theme.dim);
    let label_style = if row.selected {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
    };
    let mut prefix_width = marker.chars().count();
    let mut spans = vec![Span::styled(marker.to_owned(), marker_style)];

    if let Some(number) = row.number {
        let prefix = format!("{number}. ");

        prefix_width += prefix.chars().count();
        spans.push(Span::styled(prefix, dim));
    }

    if let Some(checked) = row.checked {
        let (prefix, style) = if checked {
            ("[✓] ", Style::default().fg(theme.accent))
        } else {
            ("[ ] ", dim)
        };

        prefix_width += prefix.chars().count();
        spans.push(Span::styled(prefix.to_owned(), style));
    }

    let label_width = width.saturating_sub(prefix_width).max(1);
    let label_lines = wrap_words(row.label, label_width);
    let mut first = Line::from(spans);

    first.spans.push(Span::styled(
        label_lines.first().cloned().unwrap_or_default(),
        label_style,
    ));

    let single_label = label_lines.len() == 1;

    if single_label && !row.description.is_empty() {
        let used = prefix_width + label_lines[0].chars().count();
        let separator = "  – ";
        let available = width.saturating_sub(used);

        if available > separator.chars().count() + 8 {
            let mut chunks = wrap_words(row.description, available - separator.chars().count());
            let head = chunks.remove(0);

            first.spans.push(Span::styled(separator.to_owned(), dim));
            first.spans.push(Span::styled(head, dim));
            body.push(first);

            for chunk in chunks {
                body.push(desc_line(theme, prefix_width, chunk));
            }

            return;
        }
    }

    body.push(first);

    for chunk in label_lines.iter().skip(1) {
        body.push(Line::from(vec![
            Span::raw(" ".repeat(prefix_width)),
            Span::styled(chunk.clone(), label_style),
        ]));
    }

    if !row.description.is_empty() {
        for chunk in wrap_words(row.description, width.saturating_sub(prefix_width).max(1)) {
            body.push(desc_line(theme, prefix_width, chunk));
        }
    }
}

fn desc_line(theme: &Theme, indent: usize, text: String) -> Line<'static> {
    Line::from(vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(text, Style::default().fg(theme.dim)),
    ])
}

/// Wrap prose on word boundaries. Words wider than the line are hard-wrapped.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split_whitespace() {
        let word_width = word.chars().count();

        if current_width == 0 && word_width > width {
            let mut chunks = composer_lines(word, width);

            if let Some(last) = chunks.pop() {
                lines.extend(chunks);
                current_width = last.chars().count();
                current = last;
            }

            continue;
        }

        let needed = if current_width == 0 {
            word_width
        } else {
            current_width + 1 + word_width
        };

        if needed <= width {
            if current_width > 0 {
                current.push(' ');
                current_width += 1;
            }

            current.push_str(word);
            current_width += word_width;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_width = word_width;
        }
    }

    if current_width > 0 || lines.is_empty() {
        lines.push(current);
    }

    lines
}

fn tab_line(progress: &QuestionProgress, theme: &Theme) -> Line<'static> {
    let accent = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.dim);
    let mut spans = Vec::new();

    for (index, question) in progress.request.questions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }

        let label = elide(&question.header, 14);

        if index == progress.index {
            spans.push(Span::styled(format!("[{label}]"), accent));
        } else {
            spans.push(Span::styled(label, dim));
        }
    }

    spans.push(Span::raw(" "));

    if progress.review() {
        spans.push(Span::styled("[Review]", accent));
    } else {
        spans.push(Span::styled("Review", dim));
    }

    Line::from(spans)
}

fn push_prompt(body: &mut Vec<Line<'static>>, theme: &Theme, question: &Question, width: usize) {
    for line in wrap_words(&question.question, width) {
        body.push(Line::from(Span::styled(
            line,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
    }
}

fn options_body(
    progress: &QuestionProgress,
    question: &Question,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut body = Vec::new();

    for (index, option) in question.options.iter().enumerate() {
        push_option(
            &mut body,
            theme,
            OptionRow {
                number: Some(index + 1),
                checked: question
                    .multi_select
                    .then(|| progress.checked.contains(&index)),
                label: &option.label,
                description: &option.description,
                selected: progress.highlight == index,
            },
            width,
        );
    }

    let draft = progress.custom_draft.as_deref().unwrap_or("");

    push_option(
        &mut body,
        theme,
        OptionRow {
            number: None,
            checked: question
                .multi_select
                .then(|| progress.custom_draft.is_some()),
            label: "Type something.",
            description: draft,
            selected: progress.on_custom_row(),
        },
        width,
    );

    if question.multi_select {
        let dim = Style::default().fg(theme.dim);
        let marker_style = if progress.on_done_row() {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let enabled = !progress.checked.is_empty() || progress.custom_draft.is_some();
        let label_style = if progress.on_done_row() {
            marker_style
        } else if enabled {
            Style::default().fg(theme.fg)
        } else {
            dim
        };

        body.push(Line::from(vec![
            Span::styled(
                if progress.on_done_row() { "→ " } else { "  " },
                marker_style,
            ),
            Span::styled("Done", label_style),
        ]));
    }

    body
}

fn preview_for<'a>(progress: &QuestionProgress, question: &'a Question) -> Option<&'a str> {
    if question.multi_select || progress.is_custom() || progress.review() {
        return None;
    }

    question
        .options
        .get(progress.highlight)
        .and_then(|option| option.preview.as_deref())
        .filter(|text| !text.trim().is_empty())
}

fn split_preview(
    left: Vec<Line<'static>>,
    preview: &str,
    left_width: usize,
    right_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let rendered = crate::markdown::render(preview, right_width, theme)
        .into_iter()
        .map(|line| {
            let mut out = Line::from(line.spans);

            if let Some(band) = line.band {
                out = out.style(Style::default().bg(band));
            }

            out
        })
        .collect::<Vec<_>>();
    let rows = left.len().max(rendered.len());
    let mut body = Vec::with_capacity(rows);

    for index in 0..rows {
        let mut spans = Vec::new();
        let used = match left.get(index) {
            Some(line) => {
                let used = line.width();

                spans.extend(line.spans.clone());
                used
            }
            None => 0,
        };

        if used < left_width {
            spans.push(Span::raw(" ".repeat(left_width - used)));
        }

        spans.push(Span::styled("│", Style::default().fg(theme.dim)));

        if let Some(line) = rendered.get(index) {
            spans.extend(line.spans.clone());
        }

        body.push(Line::from(spans));
    }

    body
}

fn question_panel(
    progress: &QuestionProgress,
    theme: &Theme,
    width: u16,
    max_rows: usize,
) -> MenuPanel {
    let inner_width = width.saturating_sub(1).max(1) as usize;
    let mut body = Vec::new();
    let mut cursor = None;
    let hint;

    if progress.review() {
        if progress.request.questions.len() > 1 {
            body.push(tab_line(progress, theme));
        }

        for (index, question) in progress.request.questions.iter().enumerate() {
            let Some(answer) = progress.answers.get(index) else {
                body.push(Line::from(Span::styled(
                    format!("{}: unanswered", question.header),
                    Style::default().fg(theme.warning),
                )));

                continue;
            };

            body.push(Line::from(vec![
                Span::styled(
                    format!("{}: ", question.header),
                    Style::default().fg(theme.dim),
                ),
                Span::styled(answer.text(question), Style::default().fg(theme.fg)),
            ]));
        }

        hint = hint_line(theme, "enter submit · ←→ questions · esc back");
    } else if let Some(question) = progress.current() {
        if progress.request.questions.len() > 1 {
            body.push(tab_line(progress, theme));
        }

        push_prompt(&mut body, theme, question, inner_width);

        if progress.is_custom() {
            body.extend(options_body(progress, question, theme, inner_width));
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "Your answer:",
                Style::default().fg(theme.dim),
            )));

            let start = body.len();

            for line in composer_lines(progress.custom.as_str(), inner_width) {
                body.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(theme.fg),
                )));
            }

            let (row, column) = cursor_offset_at(
                progress.custom.as_str(),
                progress.custom.cursor(),
                inner_width,
            );

            cursor = Some((start + row, column));
            hint = hint_line(theme, "enter submit · esc back");
        } else {
            match preview_for(progress, question) {
                Some(preview) if inner_width >= 72 => {
                    let left_width = (inner_width * 2 / 5)
                        .clamp(28, 46)
                        .min(inner_width.saturating_sub(31));
                    let right_width = inner_width.saturating_sub(left_width + 1).max(1);
                    let left = options_body(progress, question, theme, left_width);

                    body.extend(split_preview(left, preview, left_width, right_width, theme));
                }
                _ => body.extend(options_body(progress, question, theme, inner_width)),
            }

            hint = hint_line(
                theme,
                if question.multi_select {
                    "↑↓ navigate · enter or space toggle · 1-4 toggle · t custom · ←→ questions · esc back"
                } else {
                    "↑↓ navigate · enter select · 1-4 select · t custom · ←→ questions · esc back"
                },
            );
        }
    } else {
        hint = hint_line(theme, "enter select · esc cancel");
    }

    MenuPanel { body, hint, cursor }.clipped(max_rows, theme)
}

fn plan_panel(choice: &PlanChoice, theme: &Theme, width: u16, max_rows: usize) -> MenuPanel {
    let inner_width = width.saturating_sub(1).max(1) as usize;
    let rows = [
        ("Implement here", "switch to coding mode and start now"),
        ("Implement in a new session", "keep this session untouched"),
        ("Stay in plan mode", "reply with changes instead"),
    ];
    let mut body = vec![Line::from(Span::styled(
        "Plan ready",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ))];

    for (index, (label, description)) in rows.iter().enumerate() {
        push_option(
            &mut body,
            theme,
            OptionRow {
                number: Some(index + 1),
                checked: None,
                label,
                description,
                selected: index == choice.highlight,
            },
            inner_width,
        );
    }

    MenuPanel {
        body,
        hint: hint_line(
            theme,
            "↑↓ navigate · enter select · 1-3 select · esc stay in plan mode",
        ),
        cursor: None,
    }
    .clipped(max_rows, theme)
}

fn hint_line(theme: &Theme, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::default().fg(theme.dim),
    ))
}
fn render_transcript(
    frame: &mut Frame,
    area: Rect,
    state: &mut AppState,
    layout: &mut TranscriptLayout,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let height = area.height as usize;
    let previous = layout.previous_total();
    let previous_height = state.transcript_height;
    // Capture the block under the top row before the layout changes, so a
    // toggle that removes or adds rows keeps that text in place.
    let anchor = if state.scroll > 0 {
        let top = previous.saturating_sub(state.scroll.saturating_add(previous_height));

        layout.anchor_at(top)
    } else {
        None
    };

    layout.sync(state, area.width as usize);

    let total = layout.total_lines();
    let max_scroll = total.saturating_sub(height);

    state.transcript_height = height;
    state.max_scroll = max_scroll;

    // While the user is scrolled up, keep the anchored row at the top as lines
    // arrive or a display toggle changes the transcript height. Following the
    // tail (`scroll == 0`) keeps tracking new content.
    if state.scroll > 0 {
        let moved = anchor
            .as_ref()
            .and_then(|anchor| layout.line_of_anchor(anchor))
            .map(|line| total.saturating_sub(line.saturating_add(height)))
            .unwrap_or_else(|| state.scroll.saturating_add(total.saturating_sub(previous)));

        state.scroll = moved.min(max_scroll);
    } else {
        state.scroll = 0;
    }

    layout.record_total(total);

    let end = total - state.scroll;
    let start = end.saturating_sub(height);
    let visible = layout.visible_lines(state, start, end);

    state.transcript_area = area;
    state.transcript_start = start;

    let lines = visible
        .iter()
        .map(|(line, _)| line.clone())
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(lines), area);

    // Preview pixels are drawn over their reserved blank rows. The cached
    // protocol already has the target cell size; a viewport edge clips it.
    for placement in &layout.image_placements {
        let Some(image_start) = layout.image_start(placement) else {
            continue;
        };
        let image_end = image_start.saturating_add(placement.rows as usize);

        if image_start >= end || image_end <= start {
            continue;
        }

        let visible_start = image_start.max(start);
        let visible_end = image_end.min(end);
        let height = (visible_end - visible_start) as u16;
        let width = placement.columns.min(area.width.saturating_sub(1));

        if height == 0 || width == 0 {
            continue;
        }

        let Some(PreviewOutcome::Ready { protocol, .. }) = layout.previews.outcome(&placement.key)
        else {
            continue;
        };

        let rect = Rect::new(
            area.x + 1,
            area.y + (visible_start - start) as u16,
            width,
            height,
        );

        frame.render_widget(
            ratatui_image::Image::new(protocol).allow_clipping(true),
            rect,
        );
    }

    let selected = state.selection.map(|selection| selection.ordered());

    // Paint full-width message and tool bands after the text so both styled and
    // padding cells share the color, then reverse the selection on top.
    for (offset, (_, band)) in visible.iter().enumerate() {
        let y = area.y + offset as u16;
        let line_index = start + offset;

        if let Some(bg) = band {
            for x in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                    cell.set_style(Style::default().bg(*bg));
                }
            }
        }

        let Some((from, to)) = selected else {
            continue;
        };

        if line_index < from.line || line_index > to.line {
            continue;
        }

        let width = area.width as usize;
        let begin = if line_index == from.line {
            from.column
        } else {
            0
        };
        let finish = if line_index == to.line {
            to.column
        } else {
            width
        };

        for column in begin.min(width)..finish.min(width) {
            if let Some(cell) = frame.buffer_mut().cell_mut((area.x + column as u16, y)) {
                cell.modifier |= Modifier::REVERSED;
            }
        }
    }
}

/// Up to six command rows plus the key hint.
const COMPLETION_ROWS: usize = 6;

fn completion_popup_height(state: &AppState) -> u16 {
    if !state.completion_visible() || state.completions.is_empty() {
        return 0;
    }

    (state.completions.len().min(COMPLETION_ROWS) + 1) as u16
}

fn render_completions(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height <= 1 || area.width == 0 || !state.completion_visible() {
        return;
    }

    let theme = &state.theme;
    let width = area.width.saturating_sub(2) as usize;
    let selected = state.completion.unwrap_or(0);
    let start = selected.saturating_sub(COMPLETION_ROWS.saturating_sub(1));
    let end = (start + COMPLETION_ROWS).min(state.completions.len());
    let mut lines = Vec::new();

    for (position, completion) in state.completions[start..end].iter().enumerate() {
        let label = completion.label();
        let hint = completion.hint();
        let chosen = start + position == selected;
        let name_style = if chosen {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let description_style = Style::default().fg(theme.dim);
        let prefix = if chosen { "> " } else { "  " };
        let hint_width = hint.chars().count() + usize::from(!hint.is_empty());
        let description_width = width
            .saturating_sub(prefix.len())
            .saturating_sub(label.chars().count())
            .saturating_sub(hint_width)
            .saturating_sub(2);

        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(theme.dim)),
            Span::styled(label, name_style),
            Span::styled(
                if hint.is_empty() {
                    String::new()
                } else {
                    format!(" {hint}")
                },
                description_style,
            ),
            Span::raw("  "),
            Span::styled(
                elide(completion.description(), description_width),
                description_style,
            ),
        ]));
    }

    lines.push(Line::from(Span::styled(
        elide(
            if state.completion_is_mention() {
                "Tab complete · Enter accept · Esc dismiss"
            } else {
                "Tab complete · Enter run · Esc dismiss"
            },
            area.width as usize,
        ),
        Style::default().fg(theme.dim),
    )));

    frame.render_widget(Block::default().style(panel_style(theme)), area);
    frame.render_widget(Paragraph::new(lines), area);
}

/// Composer text wrapped to the terminal width, plus the cursor position in
/// the wrapped result. Built once per frame and shared by the layout pass and
/// the input widget.
struct ComposerLayout {
    lines: Vec<String>,
    row: usize,
    column: usize,
}

impl ComposerLayout {
    fn height(&self) -> u16 {
        (self.row + 1).clamp(1, 6) as u16
    }
}

fn composer_layout(input: &str, cursor: usize, width: usize) -> ComposerLayout {
    let width = width.max(1);
    let (row, column) = cursor_offset_at(input, cursor, width);

    ComposerLayout {
        lines: composer_lines(input, width),
        row,
        column,
    }
}

/// Rows the prompt box needs, capped at six.
#[cfg(test)]
fn composer_rows(input: &str, cursor: usize, width: usize) -> u16 {
    let width = width.max(1);
    let (row, _) = cursor_offset_at(input, cursor, width);

    (row + 1).clamp(1, 6) as u16
}

/// Hard-wrap one logical composer line at `width` characters.
///
/// A running column counter keeps this linear in the input; recounting the
/// current line for every character made it quadratic in the width.
pub fn composer_lines(input: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();

    for logical in input.split('\n') {
        if logical.is_empty() {
            lines.push(String::new());

            continue;
        }

        let mut current = String::with_capacity(width.min(logical.len()));
        let mut column = 0;

        for character in logical.chars() {
            if column == width {
                lines.push(std::mem::take(&mut current));
                current.reserve(width);
                column = 0;
            }

            current.push(character);
            column += 1;
        }

        lines.push(current);
    }

    lines
}

fn render_input(frame: &mut Frame, area: Rect, composer: &ComposerLayout) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let lines = &composer.lines;
    let (row, column) = (composer.row, composer.column);
    let height = area.height as usize;
    // Keep the cursor row inside the visible window as the composer grows.
    let first = row.saturating_sub(height.saturating_sub(1));
    let visible = lines[first.min(lines.len())..]
        .iter()
        .map(|line| Line::from(line.as_str()))
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(visible), area);

    let y = area.y + (row.saturating_sub(first)).min(area.height.saturating_sub(1) as usize) as u16;
    let x = area.x + (column as u16).min(area.width.saturating_sub(1));

    frame.set_cursor_position(Position::new(x, y));
}

fn render_rule(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let rule = "─".repeat(area.width as usize);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            rule,
            Style::default().fg(state.theme.rule),
        ))),
        area,
    );
}

fn render_notice(frame: &mut Frame, area: Rect, state: &AppState, notice: &str) {
    if area.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            elide(notice, area.width as usize),
            Style::default().fg(state.theme.dim),
        ))),
        area,
    );
}

/// The process notice is its own line above the queued follow-ups.
fn queue_notice(state: &AppState, width: usize) -> Option<String> {
    let processes = state.processes.len();
    let agents = state
        .agents
        .iter()
        .filter(|agent| agent.status == "running")
        .count();
    if processes == 0 && agents == 0 {
        return None;
    }

    let mut parts = Vec::new();

    if processes > 0 {
        parts.push(format!(
            "{processes} process{} running",
            if processes == 1 { "" } else { "es" }
        ));
    }

    if agents > 0 {
        parts.push(format!(
            "{agents} agent{} running",
            if agents == 1 { "" } else { "s" }
        ));
    }

    Some(elide(&parts.join(" · "), width.max(1)))
}

/// One rendered todo row.
struct TodoLine {
    marker: &'static str,
    marker_style: Style,
    text: String,
    text_style: Style,
}

/// One compact line per pending image, wrapped to the terminal width.
fn attachment_tray_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    if state.pending_attachments.is_empty() || width == 0 {
        return Vec::new();
    }

    let dim = Style::default().fg(state.theme.dim);
    let labels = state
        .pending_attachments
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let name = image
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| image.path.display().to_string());

            format!(
                "{} {} {}x{} {} {name}",
                index + 1,
                crate::images::media_label(&image.media_type),
                image.width,
                image.height,
                crate::images::human_bytes(image.bytes),
            )
        })
        .collect::<Vec<_>>()
        .join("  ·  ");
    let text = format!(" attach: {labels}");
    let width = width.max(1);

    wrap_ranges(&text, width)
        .into_iter()
        .map(|(start, end)| Line::from(Span::styled(text[start..end].to_owned(), dim)))
        .collect()
}

fn render_attachment_tray(frame: &mut Frame, area: Rect, lines: &[Line<'static>]) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    frame.render_widget(Paragraph::new(lines.to_vec()), area);
}

/// The task graph as a checklist above the composer.
///
/// This is the Pi shape: a `Todos (done/total)` header and one line per task,
/// with a filled dot for the task in progress. It replaces the one-line count so
/// progress is readable without running `/tasks`.
///
/// Returns nothing when there are no tasks, and nothing for a graph where every
/// task is complete, so a finished plan stops taking screen space.
fn todo_block(state: &AppState, width: usize, height: u16) -> Vec<TodoLine> {
    if state.tasks.is_empty() {
        return Vec::new();
    }

    let done = state
        .tasks
        .iter()
        .filter(|task| task.state == "completed")
        .count();

    // A fully finished graph no longer needs to occupy the screen.
    if done == state.tasks.len() {
        return Vec::new();
    }

    let theme = &state.theme;
    let mut lines = vec![TodoLine {
        marker: "",
        marker_style: Style::default(),
        text: format!("Todos ({done}/{})", state.tasks.len()),
        text_style: Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    }];

    // Leave room for the header and the composer; the tail is the useful end.
    let budget = height.saturating_sub(12).max(3) as usize;
    let shown = state.tasks.len().min(budget);
    let hidden = state.tasks.len() - shown;

    for task in state.tasks.iter().take(shown) {
        let (marker, marker_style, text_style) = match task.state.as_str() {
            "completed" => (
                "✓",
                Style::default().fg(theme.dim),
                Style::default().fg(theme.dim),
            ),
            "cancelled" => (
                "✗",
                Style::default().fg(theme.error),
                Style::default().fg(theme.dim),
            ),
            "in_progress" => (
                "●",
                Style::default().fg(theme.warning),
                Style::default().fg(theme.fg),
            ),
            _ => (
                "○",
                Style::default().fg(theme.dim),
                Style::default().fg(theme.fg),
            ),
        };
        let mut text = task.title.clone();

        if let Some(owner) = &task.owner {
            text.push_str(&format!(" ({owner})"));
        }

        if task.blocked {
            text.push_str(" [blocked]");
        }

        lines.push(TodoLine {
            marker,
            marker_style,
            text,
            text_style,
        });
    }

    if hidden > 0 {
        lines.push(TodoLine {
            marker: "",
            marker_style: Style::default(),
            text: format!("… {hidden} more"),
            text_style: Style::default().fg(theme.dim),
        });
    }

    let _ = width;

    lines
}

/// Draw the todo checklist.
fn render_todos(frame: &mut Frame, area: Rect, todos: &[TodoLine]) {
    let width = area.width as usize;

    let lines = todos
        .iter()
        .map(|todo| {
            let text = elide(&todo.text, width.saturating_sub(3).max(1));

            if todo.marker.is_empty() {
                Line::from(vec![Span::raw(" "), Span::styled(text, todo.text_style)])
            } else {
                Line::from(vec![
                    Span::styled(format!(" {}", todo.marker), todo.marker_style),
                    Span::raw(" "),
                    Span::styled(text, todo.text_style),
                ])
            }
        })
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(lines), area);
}

/// At most this many queued follow-ups render verbatim; the rest are counted.
pub const FOLLOW_UP_ROWS: usize = 3;

/// Pending steers and follow-ups, one line each, plus a hint that says how to
/// get the follow-ups back into the composer.
fn follow_up_lines(state: &AppState, width: usize) -> Vec<(String, bool)> {
    let steering = state.pending_steering();
    let queued = &state.queue;

    if steering.is_empty() && queued.is_empty() {
        return Vec::new();
    }

    let width = width.max(1);
    let mut rows = steering
        .iter()
        .map(|text| (format!("Steering: {text}"), true))
        .chain(
            queued
                .iter()
                .map(|text| (format!("Follow-up: {text}"), false)),
        )
        .collect::<Vec<_>>();
    let hidden = rows.len().saturating_sub(FOLLOW_UP_ROWS);

    rows.truncate(FOLLOW_UP_ROWS);

    let mut lines = rows
        .into_iter()
        .map(|(text, steering)| (elide(&text, width), steering))
        .collect::<Vec<_>>();
    let mut hint = String::new();

    if state.queue_paused {
        hint.push_str("held · ");
    }

    if hidden > 0 {
        hint.push_str(&format!("+{hidden} more · "));
    }

    hint.push_str("↳ alt+up to edit pending messages");

    if !hint.is_empty() {
        lines.push((elide(&hint, width), false));
    }

    lines
}

fn render_follow_ups(frame: &mut Frame, area: Rect, state: &AppState, lines: &[(String, bool)]) {
    let theme = &state.theme;
    let rendered = lines
        .iter()
        .map(|(line, steering)| {
            let mut style = Style::default().fg(theme.dim);

            if *steering {
                style = style.add_modifier(Modifier::ITALIC);
            }

            Line::from(Span::styled(line.clone(), style))
        })
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(rendered), area);
}

fn render_login(frame: &mut Frame, area: Rect, state: &AppState, login: &crate::state::LoginModal) {
    let theme = &state.theme;
    let width = area.width.saturating_sub(8).clamp(30, 72).min(area.width);
    let height = 10.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let title = Style::default()
        .fg(theme.title)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.dim);
    let text = Style::default().fg(theme.fg);
    let source = match state.auth {
        crate::bridge::AuthSnapshot::Environment => "environment",
        crate::bridge::AuthSnapshot::Stored => "stored in auth.json",
        crate::bridge::AuthSnapshot::Missing => "not connected",
    };
    let masked = login.masked();
    let lines = vec![
        Line::from(Span::styled(format!("login {}", login.provider), title)),
        Line::from(""),
        Line::from(vec![
            Span::styled("source: ", dim),
            Span::styled(source, text),
        ]),
        Line::from(Span::styled(
            "OPENROUTER_API_KEY overrides a stored key while it is set",
            dim,
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("key: ", dim),
            Span::styled(format!("{masked}▏"), text),
        ]),
        Line::from(""),
        Line::from(Span::styled("Enter save · Esc cancel", dim)),
    ];

    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(Block::default().style(panel_style(theme)), popup);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

pub fn display_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);

    display_path_in(path, home.as_deref())
}

fn display_path_in(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = path.strip_prefix(home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_owned();
        }

        return format!("~/{}", rest.display());
    }

    path.display().to_string()
}

/// Compact token counts; unknown totals stay visibly unknown.
///
/// One decimal is kept below ten units and dropped above it, so `1.7k` and
/// `43k` both stay short.
pub fn format_tokens(tokens: Option<u64>) -> String {
    fn scaled(value: u64, unit: u64, suffix: &str) -> String {
        let scaled = value as f64 / unit as f64;

        if scaled < 10.0 {
            format!("{scaled:.1}{suffix}")
        } else {
            format!("{scaled:.0}{suffix}")
        }
    }

    match tokens {
        None => "?".to_owned(),
        Some(tokens) if tokens >= 1_000_000 => scaled(tokens, 1_000_000, "M"),
        Some(tokens) if tokens >= 1_000 => scaled(tokens, 1_000, "k"),
        Some(tokens) => tokens.to_string(),
    }
}

fn elide(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    if text.chars().count() <= width {
        return text.to_owned();
    }

    let mut shortened: String = text.chars().take(width.saturating_sub(1)).collect();

    shortened.push('…');
    shortened
}

/// The usage line: totals, cache hit rate, cost, and context pressure.
///
/// Every part is optional. An unknown or zero value is omitted rather than
/// rendered as a zero, because a fake zero is worse than no number.
fn usage_summary(state: &AppState) -> String {
    let mut parts = Vec::new();

    if let Some(input) = state.usage.input_tokens.filter(|tokens| *tokens > 0) {
        parts.push(format!("↑{}", format_tokens(Some(input))));
    }

    if let Some(output) = state.usage.output_tokens.filter(|tokens| *tokens > 0) {
        parts.push(format!("↓{}", format_tokens(Some(output))));
    }

    if let Some(rate) = state.usage.cache_hit_rate.filter(|rate| *rate > 0.0) {
        parts.push(format!("CH{:.1}%", rate * 100.0));
    }

    if let Some(cost) = state
        .usage
        .cost_usd
        .parse::<f64>()
        .ok()
        .filter(|cost| *cost > 0.0)
    {
        parts.push(format!("${cost:.3}"));
    }

    match (state.usage.context_used, state.context_limit) {
        (Some(used), Some(limit)) if limit > 0 && used > 0 => parts.push(format!(
            "{:.1}%/{}",
            used as f64 / limit as f64 * 100.0,
            format_tokens(Some(limit))
        )),
        (None, Some(limit)) if limit > 0 => {
            parts.push(format!("?/{}", format_tokens(Some(limit))));
        }
        (Some(used), None) if used > 0 => parts.push(format_tokens(Some(used))),
        _ => {}
    }

    parts.join(" ")
}

fn render_footer(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let theme = &state.theme;
    let wide = area.width >= 80;
    let half = area.width / 2;
    let first = Rect { height: 1, ..area };
    let second = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1).min(1),
        ..area
    };
    let left = Rect {
        width: if wide { half } else { area.width },
        ..first
    };
    let right = Rect {
        x: area.x + half,
        width: area.width.saturating_sub(half),
        ..first
    };
    let mut location = display_path(&state.header.launch_dir);

    if let Some(branch) = &state.header.branch {
        location.push_str(&format!(" ({branch})"));
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            elide(&location, left.width as usize),
            Style::default().fg(theme.dim),
        ))),
        left,
    );

    if wide {
        let mode = state.header.mode.label();
        let separator = " · ";
        let reasoning = if state.reasoning_effort == t8ful_core::provider::ReasoningEffort::Default
        {
            String::new()
        } else {
            format!(" · reasoning {}", state.reasoning_effort.label())
        };
        let reserved = mode.chars().count() + separator.chars().count() + reasoning.chars().count();
        let model_missing = state.header.model.trim().is_empty();
        let model_text = if model_missing {
            "no model".to_owned()
        } else {
            state.header.model.clone()
        };
        let model = elide(&model_text, (right.width as usize).saturating_sub(reserved));
        let model_style = if model_missing {
            Style::default().fg(theme.dim)
        } else {
            Style::default().fg(theme.fg)
        };
        let mode_color = match state.header.mode {
            t8ful_core::policy::Mode::Plan => theme.warning,
            t8ful_core::policy::Mode::Coding => theme.accent,
        };
        let mut spans = vec![Span::styled(model, model_style)];

        if !reasoning.is_empty() {
            spans.push(Span::styled(reasoning, Style::default().fg(theme.dim)));
        }

        spans.push(Span::styled(separator, Style::default().fg(theme.dim)));
        spans.push(Span::styled(mode, Style::default().fg(mode_color)));

        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
            right,
        );
    }

    if second.height == 0 {
        return;
    }

    let second_left = Rect {
        width: if wide { half } else { area.width },
        ..second
    };
    let second_right = Rect {
        x: area.x + half,
        width: area.width.saturating_sub(half),
        ..second
    };

    let usage = usage_summary(state);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            elide(&usage, second_left.width as usize),
            Style::default().fg(theme.dim),
        ))),
        second_left,
    );

    if wide {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "? help",
                Style::default().fg(theme.dim),
            )))
            .alignment(Alignment::Right),
            second_right,
        );
    }
}

/// Number of trailing tool-body lines kept while output is collapsed.
pub const COLLAPSED_TOOL_LINES: usize = 4;

/// Preview bodies collapse to their first lines; `ctrl+o` shows every line.
pub const PREVIEW_COLLAPSED_LINES: usize = 5;

/// Compact startup hints; the banner is seeded at the start of every session.
pub const BANNER_HINTS: [&str; 3] = [
    "escape interrupt · ctrl+c cancel/clear · ctrl+d quit · / commands",
    "enter send · tab plan/code · ctrl+q queue · ctrl+v paste image",
    "ctrl+o expand tool output · ctrl+r reasoning · alt+e level · drag to copy",
];

pub const BANNER_DESCRIPTION: &str = "Thoughts matter.";

#[derive(Debug, Default)]
struct EntryLayout {
    lines: Vec<Line<'static>>,
    bands: Vec<Option<Color>>,
    /// Columns of leading gutter that are not transcript text.
    gutter: usize,
}

/// One image reserved inside the committed transcript.
#[derive(Debug, Clone)]
struct ImagePlacement {
    /// Index of the entry that owns the image, in commit order.
    entry: usize,
    /// Row offset inside that entry's rendered lines.
    offset: usize,
    columns: u16,
    rows: u16,
    key: PreviewKey,
}

/// Identity of one block that is still growing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LiveKey {
    Reasoning,
    Assistant,
    Tool(String),
}

/// One rendered block that is not committed to the transcript yet.
#[derive(Debug)]
struct LiveBlock {
    lines: Vec<Line<'static>>,
    bands: Vec<Option<Color>>,
    gutter: usize,
    generation: u64,
    width: usize,
}

/// One live block placed in the transcript.
///
/// A tool card carries the dispatch position of its call; the reasoning and
/// assistant streams have none and always follow the committed transcript.
#[derive(Debug)]
struct LiveSpan {
    key: LiveKey,
    placement: Option<crate::state::CardPlacement>,
    rows: usize,
    /// First line of the span, including its separator.
    start: usize,
    /// Whether a blank separator precedes the span.
    gap: bool,
}

/// What one segment of the display order renders.
#[derive(Debug, Clone, Copy)]
enum SegmentKind {
    Entry(usize),
    Live(usize),
}

/// One block in display order: a committed entry or a live span.
#[derive(Debug, Clone, Copy)]
struct Segment {
    kind: SegmentKind,
    /// First line of the segment, including its separator.
    start: usize,
    gap: bool,
    rows: usize,
}

impl Segment {
    fn end(&self) -> usize {
        self.start + usize::from(self.gap) + self.rows
    }

    fn content_start(&self) -> usize {
        self.start + usize::from(self.gap)
    }
}

/// What a rendered line index resolves to.
#[derive(Debug, Clone, Copy)]
enum ResolvedLine {
    Blank,
    Entry { index: usize, offset: usize },
    Live { index: usize, offset: usize },
}

/// A stable point in the transcript, used to keep the viewport in place while
/// the transcript grows or a display toggle changes its height.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Anchor {
    Entry { index: usize, offset: usize },
    Live { key: LiveKey, offset: usize },
}

/// One growing markdown block, rendered incrementally while text arrives.
#[derive(Debug)]
struct StreamingBlock {
    renderer: crate::markdown::StreamingRenderer,
    generation: u64,
    width: usize,
}

/// Cached transcript layout. Committed entries and live blocks are rebuilt only
/// when their generation, the width, or the palette changes; a display-only
/// toggle rebuilds just the entries it affects, and tool bodies are wrapped
/// once and reused while scrolling.
#[derive(Debug)]
pub struct TranscriptLayout {
    width: usize,
    /// Background image decoder and its bounded preview cache.
    previews: ImagePreviews,
    /// Absolute transcript rows reserved by image blocks.
    image_placements: Vec<ImagePlacement>,
    /// Session that owns the cached previews.
    image_session: String,
    content_width: usize,
    entries_generation: u64,
    entries_len: usize,
    show_reasoning: bool,
    expanded: bool,
    theme: Theme,
    busy: bool,
    entries: Vec<EntryLayout>,
    /// Rendered blocks in display order. Committed cards are hoisted to the
    /// transcript position where their call started; everything else stays at
    /// its commit position.
    segments: Vec<Segment>,
    /// Segment of each cached entry, in commit order.
    entry_segments: Vec<usize>,
    /// Live blocks still growing, in the order they were collected.
    live_spans: Vec<LiveSpan>,
    /// Segment of each live span.
    live_segments: Vec<usize>,
    live_cache: std::collections::HashMap<LiveKey, LiveBlock>,
    /// Wrapping of each running tool body, so a growing command re-wraps only
    /// the bytes that arrived since the last frame.
    live_wrap: std::collections::HashMap<LiveKey, WrapCache>,
    /// The streaming assistant message and the live reasoning block. Their rows
    /// live in an incremental markdown renderer rather than the block cache, so
    /// a delta re-renders only the block that is still growing.
    assistant: Option<StreamingBlock>,
    reasoning_stream: Option<StreamingBlock>,
    /// Total lines from the previous render, for scroll anchoring.
    previous_total: usize,
    #[cfg(test)]
    entry_builds: usize,
    #[cfg(test)]
    live_builds: usize,
}

impl Default for TranscriptLayout {
    fn default() -> Self {
        Self {
            width: 0,
            previews: ImagePreviews::new(),
            image_placements: Vec::new(),
            image_session: String::new(),
            content_width: 0,
            entries_generation: 0,
            entries_len: 0,
            show_reasoning: false,
            expanded: false,
            theme: Theme::pi(),
            busy: false,
            entries: Vec::new(),
            segments: Vec::new(),
            entry_segments: Vec::new(),
            live_spans: Vec::new(),
            live_segments: Vec::new(),
            live_cache: std::collections::HashMap::new(),
            live_wrap: std::collections::HashMap::new(),
            assistant: None,
            reasoning_stream: None,
            previous_total: 0,
            #[cfg(test)]
            entry_builds: 0,
            #[cfg(test)]
            live_builds: 0,
        }
    }
}

impl TranscriptLayout {
    /// Bring the cached layout in line with the current state and width.
    pub fn sync(&mut self, state: &AppState, width: usize) {
        let width = width.max(1);
        let content_width = width.saturating_sub(2).max(8);
        let generation = state.entries_generation();
        let session_changed = self.image_session != state.header.session_id;

        if session_changed {
            self.image_session.clone_from(&state.header.session_id);
            self.previews.invalidate();
        }

        let invalidate = session_changed
            || self.width != width
            || self.entries_generation != generation
            || state.entries.len() < self.entries_len
            || self.theme != state.theme;

        if invalidate {
            self.width = width;
            self.content_width = content_width;
            self.entries.clear();
            self.segments.clear();
            self.entry_segments.clear();
            self.live_segments.clear();
            self.entries_len = 0;
            self.entries_generation = generation;
            self.show_reasoning = state.show_reasoning;
            self.expanded = state.expanded;
            self.theme = state.theme;
            self.live_cache.clear();
            self.live_wrap.clear();
            self.assistant = None;
            self.reasoning_stream = None;
            self.image_placements.clear();
        }

        for entry in &state.entries[self.entries_len..] {
            if let Entry::Images(images) = entry {
                self.push_images_entry(images);
            } else {
                self.push_entry(build_entry(
                    entry,
                    self.content_width,
                    &self.theme,
                    self.show_reasoning,
                    self.expanded,
                ));
            }
        }

        self.entries_len = state.entries.len();
        self.busy = state.busy;

        // A display-only toggle changes how a subset of entries render. Rebuild
        // only that subset; the line index is rebuilt once at the end of the
        // sync, so ctrl-o and ctrl-r stay cheap on a long transcript.
        if self.expanded != state.expanded {
            self.expanded = state.expanded;
            let width = self.content_width;
            self.rebuild_entries(state, |entry| expanded_affects(entry, width));
            // Running tool cards are windowed by the same flag.
            self.live_cache
                .retain(|key, _| !matches!(key, LiveKey::Tool(_)));
            self.live_wrap.clear();
        }

        if self.show_reasoning != state.show_reasoning {
            self.show_reasoning = state.show_reasoning;
            self.rebuild_entries(state, |entry| matches!(entry, Entry::Reasoning(_)));
        }

        self.sync_live(state);
    }

    /// Append an image block, reserving its rows and registering the widget
    /// rectangles for the renderer.
    fn push_images_entry(&mut self, images: &[t8ful_core::content::ImageInput]) {
        let entry = self.entries.len();
        let (layout, placements) =
            images_layout(images, self.content_width, &self.theme, &mut self.previews);

        self.push_entry(layout);

        for (offset, key, columns, rows) in placements {
            self.image_placements.push(ImagePlacement {
                entry,
                offset,
                columns,
                rows,
                key,
            });
        }
    }

    /// Apply finished decodes and rebuild entry layouts when one became ready.
    pub fn apply_image_results(&mut self) -> bool {
        if !self.previews.apply_results() {
            return false;
        }

        self.entries.clear();
        self.segments.clear();
        self.entry_segments.clear();
        self.live_segments.clear();
        self.entries_len = 0;
        self.image_placements.clear();

        true
    }

    pub fn has_pending_previews(&self) -> bool {
        self.previews.has_pending()
    }

    /// Select a graphics protocol from the environment. This never reads
    /// stdin, so it cannot race the terminal event reader.
    pub fn detect_picker(&mut self) {
        self.previews
            .set_picker(crate::images::picker_from_environment());
    }

    /// Append one committed entry's layout. Its position in the transcript is
    /// decided later, when the display order is rebuilt.
    fn push_entry(&mut self, layout: EntryLayout) {
        self.entries.push(layout);

        #[cfg(test)]
        {
            self.entry_builds += 1;
        }
    }

    /// Re-render the entries a display toggle affects.
    fn rebuild_entries(&mut self, state: &AppState, rebuild: impl Fn(&Entry) -> bool) {
        for (index, entry) in state.entries.iter().enumerate() {
            if !rebuild(entry) {
                continue;
            }

            let Some(layout) = self.entries.get_mut(index) else {
                continue;
            };

            *layout = build_entry(
                entry,
                self.content_width,
                &self.theme,
                self.show_reasoning,
                self.expanded,
            );

            #[cfg(test)]
            {
                self.entry_builds += 1;
            }
        }
    }

    /// Rebuild the live blocks and the display order that places every card
    /// where its call started.
    fn sync_live(&mut self, state: &AppState) {
        let content_width = self.content_width;
        let theme = self.theme;
        let expanded = self.expanded;
        let mut spans = Vec::new();

        for run in &state.tool_runs {
            let key = LiveKey::Tool(run.call_id.clone());
            let stale = self.live_cache.get(&key).is_none_or(|block| {
                block.generation != run.generation || block.width != content_width
            });

            if stale {
                let cache = self.live_wrap.entry(key.clone()).or_default();
                let mut content = match (&run.result, run.progress.is_empty()) {
                    (Some(result), true) => result.content.clone(),
                    (Some(result), false) => format!("{}\n{}", result.content, run.progress),
                    (None, _) => run.progress.clone(),
                };

                if run.truncated && !run.progress.is_empty() {
                    content.push_str("\n[streamed output truncated]\n");
                }

                let status = run
                    .result
                    .as_ref()
                    .map(|result| result.status.as_str())
                    .unwrap_or("running");
                let body = build_tool(ToolLayout {
                    name: &run.tool,
                    status,
                    content: &content,
                    title: &run.title,
                    preview: &run.preview,
                    duration: None,
                    width: content_width,
                    theme: &theme,
                    expanded,
                    cache: Some(cache),
                });

                self.live_cache.insert(
                    key.clone(),
                    LiveBlock {
                        lines: body.lines,
                        bands: body.bands,
                        gutter: body.gutter,
                        generation: run.generation,
                        width: content_width,
                    },
                );

                #[cfg(test)]
                {
                    self.live_builds += 1;
                }
            }

            spans.push(LiveSpan {
                key,
                placement: Some(run.placement),
                rows: 0,
                start: 0,
                gap: false,
            });
        }

        if state.reasoning().is_empty() {
            self.reasoning_stream = None;
        } else if self.show_reasoning {
            if sync_stream(
                &mut self.reasoning_stream,
                state.reasoning(),
                state.reasoning_generation(),
                content_width,
                theme,
            ) {
                #[cfg(test)]
                {
                    self.live_builds += 1;
                }
            }

            // Reasoning is a stream, not a card: it follows the transcript.
            spans.push(LiveSpan {
                key: LiveKey::Reasoning,
                placement: None,
                rows: 0,
                start: 0,
                gap: false,
            });
        }

        if state.streaming.is_empty() {
            self.assistant = None;
        } else {
            if sync_stream(
                &mut self.assistant,
                &state.streaming,
                state.streaming_generation(),
                content_width,
                theme,
            ) {
                #[cfg(test)]
                {
                    self.live_builds += 1;
                }
            }

            spans.push(LiveSpan {
                key: LiveKey::Assistant,
                placement: None,
                rows: 0,
                start: 0,
                gap: false,
            });
        }

        self.live_cache
            .retain(|key, _| spans.iter().any(|span| &span.key == key));
        self.live_wrap
            .retain(|key, _| spans.iter().any(|span| &span.key == key));

        let rows = spans
            .iter()
            .map(|span| self.live_row_count(&span.key).unwrap_or(0))
            .collect::<Vec<_>>();

        for (span, rows) in spans.iter_mut().zip(rows) {
            span.rows = rows;
        }

        self.live_spans = spans;
        self.rebuild_segments(state);
    }

    /// Recompute the display order and the line index.
    ///
    /// A card renders before the entry at its anchor, the transcript position
    /// where its call was dispatched. Entries without a card placement keep
    /// their commit position, and the reasoning and assistant streams follow
    /// everything else.
    fn rebuild_segments(&mut self, state: &AppState) {
        let entries = state.entries.len();
        let mut cards = Vec::new();

        for (index, entry) in state.entries.iter().enumerate() {
            if let Some(placement) = entry.placement() {
                // An inline card has no dispatch anchor; it stays where it was
                // committed, which is its own index.
                let anchor = if placement.anchor == usize::MAX {
                    index
                } else {
                    placement.anchor.min(entries)
                };

                cards.push((anchor, placement.seq, false, index));
            }
        }

        for (index, span) in self.live_spans.iter().enumerate() {
            if let Some(placement) = span.placement {
                cards.push((placement.anchor.min(entries), placement.seq, true, index));
            }
        }

        cards.sort_by_key(|(anchor, seq, _, _)| (*anchor, *seq));

        // Reuse the buffers: this runs every frame, and a long transcript must
        // not allocate its whole index again for each draw.
        let mut segments = std::mem::take(&mut self.segments);
        let mut entry_segments = std::mem::take(&mut self.entry_segments);
        let mut live_segments = std::mem::take(&mut self.live_segments);
        let mut live_start = vec![0; self.live_spans.len()];
        let mut live_gap = vec![false; self.live_spans.len()];

        segments.clear();
        entry_segments.clear();
        entry_segments.resize(entries, usize::MAX);
        live_segments.clear();
        live_segments.resize(self.live_spans.len(), usize::MAX);

        let mut total = 0;
        let mut any = false;
        let mut cursor = 0;

        for position in 0..=entries {
            while let Some((anchor, _, live, index)) = cards.get(cursor) {
                if *anchor != position {
                    break;
                }

                let rows = if *live {
                    self.live_spans[*index].rows
                } else {
                    self.entries[*index].lines.len()
                };
                let gap = any && rows > 0;
                let start = total;

                total += usize::from(gap) + rows;
                any |= rows > 0;

                let segment = segments.len();

                segments.push(Segment {
                    kind: if *live {
                        SegmentKind::Live(*index)
                    } else {
                        SegmentKind::Entry(*index)
                    },
                    start,
                    gap,
                    rows,
                });

                if *live {
                    live_segments[*index] = segment;
                    live_start[*index] = start;
                    live_gap[*index] = gap;
                } else {
                    entry_segments[*index] = segment;
                }

                cursor += 1;
            }

            if position < entries && state.entries[position].placement().is_none() {
                let rows = self.entries[position].lines.len();
                let gap = any && rows > 0;
                let start = total;

                total += usize::from(gap) + rows;
                any |= rows > 0;

                let segment = segments.len();

                segments.push(Segment {
                    kind: SegmentKind::Entry(position),
                    start,
                    gap,
                    rows,
                });
                entry_segments[position] = segment;
            }
        }

        for (index, span) in self.live_spans.iter().enumerate() {
            if span.placement.is_some() {
                continue;
            }

            let rows = span.rows;
            let gap = any && rows > 0;
            let start = total;

            total += usize::from(gap) + rows;
            any |= rows > 0;

            let segment = segments.len();

            segments.push(Segment {
                kind: SegmentKind::Live(index),
                start,
                gap,
                rows,
            });
            live_segments[index] = segment;
            live_start[index] = start;
            live_gap[index] = gap;
        }

        for (index, span) in self.live_spans.iter_mut().enumerate() {
            span.start = live_start[index];
            span.gap = live_gap[index];
        }

        self.segments = segments;
        self.entry_segments = entry_segments;
        self.live_segments = live_segments;
    }

    /// Displayed rows of one live block, without its trailing blank lines.
    fn live_row_count(&self, key: &LiveKey) -> Option<usize> {
        let rows = match key {
            LiveKey::Assistant => self.assistant.as_ref()?.renderer.rows().len(),
            LiveKey::Reasoning => self.reasoning_stream.as_ref()?.renderer.rows().len(),
            _ => self.live_cache.get(key)?.lines.len(),
        };

        Some(rows)
    }

    /// One displayed row of a live block.
    fn live_row(&self, key: &LiveKey, offset: usize) -> Option<(Line<'static>, Option<Color>)> {
        match key {
            LiveKey::Assistant => {
                let row = self.assistant.as_ref()?.renderer.rows().get(offset)?;

                Some(assistant_row(row.clone()))
            }
            LiveKey::Reasoning => {
                let row = self
                    .reasoning_stream
                    .as_ref()?
                    .renderer
                    .rows()
                    .get(offset)?;

                Some(reasoning_row(row.clone(), &self.theme))
            }
            _ => {
                let block = self.live_cache.get(key)?;

                Some((block.lines.get(offset)?.clone(), block.bands[offset]))
            }
        }
    }

    /// Resolve one rendered line to the block and row that produced it.
    fn resolve(&self, index: usize) -> Option<ResolvedLine> {
        let position = self
            .segments
            .partition_point(|segment| segment.end() <= index);
        let segment = self.segments.get(position)?;
        let offset = index.checked_sub(segment.start)?;

        if segment.gap && offset == 0 {
            return Some(ResolvedLine::Blank);
        }

        let offset = offset.saturating_sub(usize::from(segment.gap));

        Some(match segment.kind {
            SegmentKind::Entry(index) => ResolvedLine::Entry { index, offset },
            SegmentKind::Live(index) => ResolvedLine::Live { index, offset },
        })
    }

    /// Absolute line of one image cell row, after any card above it moved it.
    fn image_start(&self, placement: &ImagePlacement) -> Option<usize> {
        let segment = self
            .segments
            .get(*self.entry_segments.get(placement.entry)?)?;

        Some(segment.content_start() + placement.offset)
    }

    /// The anchor for a rendered line. The busy line and lines past the
    /// transcript have none.
    fn anchor_at(&self, line: usize) -> Option<Anchor> {
        let position = self
            .segments
            .partition_point(|segment| segment.end() <= line);
        let segment = self.segments.get(position)?;
        let offset = line
            .saturating_sub(segment.content_start())
            .min(segment.rows.saturating_sub(1));

        Some(match &segment.kind {
            SegmentKind::Entry(index) => Anchor::Entry {
                index: *index,
                offset,
            },
            SegmentKind::Live(index) => Anchor::Live {
                key: self.live_spans.get(*index)?.key.clone(),
                offset,
            },
        })
    }

    /// The line an anchor resolves to now, or `None` when its block is gone.
    fn line_of_anchor(&self, anchor: &Anchor) -> Option<usize> {
        let segment = match anchor {
            Anchor::Entry { index, .. } => self.segments.get(*self.entry_segments.get(*index)?)?,
            Anchor::Live { key, .. } => {
                let index = self.live_spans.iter().position(|span| &span.key == key)?;

                self.segments.get(*self.live_segments.get(index)?)?
            }
        };
        let offset = match anchor {
            Anchor::Entry { offset, .. } | Anchor::Live { offset, .. } => *offset,
        };

        Some(segment.content_start() + offset.min(segment.rows.saturating_sub(1)))
    }

    /// Total number of rendered transcript lines, including live blocks and the
    /// busy line.
    pub fn total_lines(&self) -> usize {
        self.segments.last().map_or(0, Segment::end) + usize::from(self.busy)
    }

    /// Total lines produced by the previous sync, before new content arrived.
    pub fn previous_total(&self) -> usize {
        self.previous_total
    }

    /// Remember the line count of this render for the next scroll anchor.
    pub fn record_total(&mut self, total: usize) {
        self.previous_total = total;
    }

    /// Build only the requested slice of rendered lines, each with an optional
    /// full-width band color.
    pub fn visible_lines(
        &self,
        state: &AppState,
        start: usize,
        end: usize,
    ) -> Vec<(Line<'static>, Option<Color>)> {
        let total = self.total_lines();
        let end = end.min(total);
        let start = start.min(end);
        let mut lines = Vec::with_capacity(end - start);

        for index in start..end {
            let line = match self.resolve(index) {
                Some(ResolvedLine::Blank) => (Line::from(""), None),
                Some(ResolvedLine::Entry { index, offset }) => {
                    let layout = &self.entries[index];

                    (layout.lines[offset].clone(), layout.bands[offset])
                }
                Some(ResolvedLine::Live { index, offset }) => self
                    .live_row(&self.live_spans[index].key, offset)
                    .unwrap_or_else(|| (Line::from(""), None)),
                None => (busy_line(state, self.content_width), None),
            };

            lines.push(line);
        }

        lines
    }

    /// The rendered text of one line, gutters included.
    pub fn line_text(&self, index: usize) -> Option<String> {
        self.line_at(index).map(|(line, _)| line.to_string())
    }

    fn line_at(&self, index: usize) -> Option<(Line<'static>, usize)> {
        match self.resolve(index)? {
            ResolvedLine::Blank => Some((Line::from(""), 0)),
            ResolvedLine::Entry { index, offset } => {
                let layout = self.entries.get(index)?;
                let line = layout.lines.get(offset)?;

                Some((line.clone(), layout.gutter))
            }
            ResolvedLine::Live { index, offset } => {
                let key = &self.live_spans.get(index)?.key;
                let gutter = match key {
                    LiveKey::Assistant | LiveKey::Reasoning => 1,
                    _ => self.live_cache.get(key)?.gutter,
                };

                Some((self.live_row(key, offset)?.0, gutter))
            }
        }
    }

    /// Text covered by a selection, one rendered line per entry, with each
    /// block's gutter removed and trailing spaces trimmed.
    ///
    /// A range that ends at column zero does not include the trailing line: a
    /// drag that ends after the last newline reads as whole lines only.
    pub fn selection_text(&self, start: Point, end: Point) -> String {
        let end = if end.column == 0 && end.line > start.line {
            Point::new(end.line - 1, usize::MAX)
        } else {
            end
        };
        let mut lines = Vec::new();

        for index in start.line..=end.line {
            let Some((line, gutter)) = self.line_at(index) else {
                continue;
            };
            // The cell buffer drops every control character, so the copy
            // must drop the same characters before column offsets are applied.
            let chars = line
                .to_string()
                .chars()
                .filter(|character| !character.is_control())
                .skip(gutter)
                .collect::<Vec<_>>();
            let from = if index == start.line {
                start.column.saturating_sub(gutter)
            } else {
                0
            };
            let to = if index == end.line {
                end.column.saturating_sub(gutter)
            } else {
                chars.len()
            };
            let from = from.min(chars.len());
            let to = to.min(chars.len()).max(from);

            lines.push(
                chars[from..to]
                    .iter()
                    .collect::<String>()
                    .trim_end()
                    .to_owned(),
            );
        }

        lines.join("\n")
    }
}

/// Feed a streaming block its newest text, re-rendering only when it changed.
fn sync_stream(
    block: &mut Option<StreamingBlock>,
    text: &str,
    generation: u64,
    width: usize,
    theme: Theme,
) -> bool {
    let stale = block
        .as_ref()
        .is_none_or(|block| block.generation != generation || block.width != width);

    if !stale {
        return false;
    }

    if block.as_ref().is_none_or(|block| block.width != width) {
        // One column is reserved for the gutter, as in `markdown_entry`.
        *block = Some(StreamingBlock {
            renderer: crate::markdown::StreamingRenderer::new(
                width.saturating_sub(1).max(1),
                theme,
            ),
            generation,
            width,
        });
    }

    if let Some(block) = block.as_mut() {
        block.renderer.render(text);
        block.generation = generation;
    }

    true
}

fn plain_entry(
    text: &str,
    width: usize,
    prefix: &str,
    prefix_style: Style,
    style: Style,
    band: Option<Color>,
) -> EntryLayout {
    let prefix_width = prefix.chars().count();
    let body_width = width.saturating_sub(prefix_width).max(1);
    let mut lines = Vec::new();
    let mut bands = Vec::new();

    for (index, (start, end)) in wrap_ranges(text, body_width).into_iter().enumerate() {
        let lead = if index == 0 {
            prefix.to_owned()
        } else {
            " ".repeat(prefix_width)
        };

        lines.push(Line::from(vec![
            Span::styled(lead, prefix_style),
            Span::styled(text[start..end].to_owned(), style),
        ]));
        bands.push(band);
    }

    EntryLayout {
        lines,
        bands,
        gutter: prefix_width,
    }
}

fn build_entry(
    entry: &Entry,
    width: usize,
    theme: &Theme,
    show_reasoning: bool,
    expanded: bool,
) -> EntryLayout {
    match entry {
        Entry::User(text) => plain_entry(
            text,
            width,
            " ▍ ",
            Style::default().fg(theme.accent),
            Style::default().fg(theme.fg),
            theme.user_bg,
        ),
        Entry::Images(images) => images_entry(images, width, theme),
        Entry::Assistant(text) => markdown_entry(text, width, theme),
        Entry::Reasoning(text) => {
            if show_reasoning {
                reasoning_entry(text, width, theme)
            } else {
                EntryLayout::default()
            }
        }
        Entry::Tool {
            name,
            status,
            content,
            title,
            preview,
            duration,
            ..
        } => build_tool(ToolLayout {
            name,
            status,
            content,
            title,
            preview,
            duration: *duration,
            width,
            theme,
            expanded,
            cache: None,
        }),
        Entry::Info(text) => plain_entry(
            text,
            width,
            " · ",
            Style::default().fg(theme.dim),
            Style::default().fg(theme.dim),
            None,
        ),
        Entry::Error(text) => plain_entry(
            text,
            width,
            " error: ",
            Style::default().fg(theme.error),
            Style::default().fg(theme.error),
            None,
        ),
        Entry::Plan { body, mode, .. } => plan_entry(body, *mode, width, theme),
        Entry::ModeChange { from, to } => mode_change_entry(*from, *to, width, theme),
        Entry::Startup { version } => startup_entry(version, width, theme),
        Entry::Agent {
            id,
            role,
            status,
            isolation,
            task,
            worktree,
            branch,
            prompt,
            report,
        } => agent_entry(
            AgentCard {
                id,
                role,
                status,
                isolation,
                task: task.as_deref(),
                worktree: worktree.as_deref(),
                branch: branch.as_deref(),
                prompt,
                report: report.as_deref(),
            },
            width,
            theme,
            expanded,
        ),
    }
}

/// Everything an agent card draws.
struct AgentCard<'a> {
    id: &'a str,
    role: &'a str,
    status: &'a str,
    isolation: &'a str,
    task: Option<&'a str>,
    worktree: Option<&'a str>,
    branch: Option<&'a str>,
    prompt: &'a str,
    report: Option<&'a str>,
}

/// A child agent is one banded card: who it is, what it was asked, and what it
/// reported. Its own transcript stays out of the parent, so this is the only
/// thing a delegating turn adds to the page.
fn agent_entry(card: AgentCard<'_>, width: usize, theme: &Theme, expanded: bool) -> EntryLayout {
    let gutter_color = match card.status {
        "running" => theme.accent,
        "completed" => theme.success,
        "failed" => theme.error,
        _ => theme.warning,
    };
    let gutter = Style::default().fg(gutter_color);
    let label = Style::default()
        .fg(gutter_color)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.dim);
    let rule_style = Style::default().fg(theme.rule);
    let mut lines = Vec::new();

    let marker = {
        let mut marker = card.status.to_owned();

        if card.isolation == "worktree" {
            marker.push_str(" · worktree");
        }

        if let Some(task) = card.task {
            marker.push_str(&format!(" · {task}"));
        }

        marker
    };
    let heading = format!("Agent {} · {}", card.id, card.role);
    let heading_width = 1 + heading.chars().count();
    let padding = width.saturating_sub(heading_width + marker.chars().count());

    lines.push(Line::from(vec![
        Span::styled("▏", gutter),
        Span::styled(heading, label),
        Span::styled(" ".repeat(padding), dim),
        Span::styled(marker, dim),
    ]));

    lines.push(plan_rule(width, gutter, rule_style));

    let body_width = width.saturating_sub(3).max(1);
    let body = match (expanded, card.report) {
        (true, Some(report)) => format!("{}\n\n---\n\n{report}", card.prompt),
        (false, Some(report)) => report.to_owned(),
        (_, None) => card.prompt.to_owned(),
    };

    for rendered in crate::markdown::render(&body, body_width, theme) {
        let mut spans = vec![Span::styled("▏", gutter), Span::raw(" ")];

        spans.extend(rendered.spans);
        lines.push(Line::from(spans));
    }

    if let (Some(worktree), Some(branch)) = (card.worktree, card.branch) {
        lines.push(plan_rule(width, gutter, rule_style));

        let hint = format!("git -C {worktree} diff {branch}");
        let hint = elide(&hint, width.saturating_sub(3).max(1));

        lines.push(Line::from(vec![
            Span::styled("▏", gutter),
            Span::raw(" "),
            Span::styled(hint, dim),
        ]));
    }

    lines.push(plan_rule(width, gutter, rule_style));

    let bands = vec![theme.tool_bg; lines.len()];

    EntryLayout {
        lines,
        bands,
        gutter: 1,
    }
}

/// Draw one streamed markdown row like a committed assistant entry: behind the
/// same one-column gutter.
fn assistant_row((line, band): StreamRow) -> (Line<'static>, Option<Color>) {
    let mut spans = vec![Span::raw(" ")];

    spans.extend(line.spans);

    (Line::from(spans), band)
}

/// Draw one streamed reasoning row in the dim italic palette used by committed
/// reasoning, keeping only emphasis modifiers.
fn reasoning_row((line, band): StreamRow, theme: &Theme) -> (Line<'static>, Option<Color>) {
    let base = Style::default()
        .fg(theme.dim)
        .add_modifier(Modifier::ITALIC);
    let kept = Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::CROSSED_OUT;
    let mut spans = vec![Span::styled(" ", base)];

    spans.extend(line.spans.into_iter().map(|span| {
        let style = base.add_modifier(span.style.add_modifier & kept);

        Span::styled(span.content, style)
    }));

    (Line::from(spans), band)
}

fn markdown_entry(text: &str, width: usize, theme: &Theme) -> EntryLayout {
    let rendered = crate::markdown::render(text, width.saturating_sub(1).max(1), theme);
    let mut lines = Vec::new();

    for rendered_line in rendered {
        let mut spans = vec![Span::raw(" ")];

        spans.extend(rendered_line.spans);
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        lines.push(Line::from(" "));
    }

    let bands = vec![None; lines.len()];

    EntryLayout {
        lines,
        bands,
        gutter: 1,
    }
}

/// Reasoning keeps its structure but never competes with the answer: every
/// span ends up dim and italic, keeping only the emphasis the markdown asked
/// for and dropping any background the renderer applied.
fn reasoning_entry(text: &str, width: usize, theme: &Theme) -> EntryLayout {
    let markdown = markdown_entry(text, width, theme);
    let base = Style::default()
        .fg(theme.dim)
        .add_modifier(Modifier::ITALIC);
    let kept = Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::CROSSED_OUT;
    let mut lines = Vec::new();

    for line in markdown.lines {
        let spans = line
            .spans
            .into_iter()
            .map(|span| {
                let style = base.add_modifier(span.style.add_modifier & kept);

                Span::styled(span.content, style)
            })
            .collect::<Vec<_>>();

        lines.push(Line::from(spans));
    }

    let bands = vec![None; lines.len()];

    EntryLayout {
        lines,
        bands,
        gutter: 1,
    }
}

fn startup_entry(version: &str, width: usize, theme: &Theme) -> EntryLayout {
    let brand = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        Span::styled("█", brand),
        Span::styled(format!(" t8ful v{version}"), brand),
    ])];

    for hint in BANNER_HINTS {
        lines.push(Line::from(Span::styled(
            format!(" {}", elide(hint, width.saturating_sub(1))),
            Style::default().fg(theme.dim),
        )));
    }

    lines.push(Line::from(Span::styled(
        format!(" {}", elide(BANNER_DESCRIPTION, width.saturating_sub(1))),
        Style::default().fg(theme.fg),
    )));
    lines.push(Line::from(""));

    let bands = vec![None; lines.len()];

    EntryLayout {
        lines,
        bands,
        gutter: 1,
    }
}

/// A presented plan is a document, not tool output: a banded panel with an
/// amber gutter, a titled header, the markdown body, and a footer that says
/// what the block waits for. It is never collapsed, however long the plan is.
fn plan_entry(body: &str, mode: Mode, width: usize, theme: &Theme) -> EntryLayout {
    let band = theme.tool_bg;
    let gutter = Style::default().fg(theme.warning);
    let label = Style::default()
        .fg(theme.warning)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.dim);
    let rule_style = Style::default().fg(theme.rule);
    let mut lines = Vec::new();

    let marker = "· presented";
    let label_width = 1 + "Plan".chars().count();
    let padding = width.saturating_sub(label_width + marker.chars().count());

    lines.push(Line::from(vec![
        Span::styled("▏", gutter),
        Span::styled("Plan", label),
        Span::styled(" ".repeat(padding), dim),
        Span::styled(marker, dim),
    ]));

    lines.push(plan_rule(width, gutter, rule_style));

    let body_width = width.saturating_sub(3).max(1);
    let rendered = crate::markdown::render(body, body_width, theme);

    if rendered.is_empty() {
        lines.push(Line::from(vec![Span::styled("▏", gutter)]));
    } else {
        for rendered in rendered {
            let mut spans = vec![Span::styled("▏", gutter), Span::raw(" ")];

            spans.extend(rendered.spans);
            lines.push(Line::from(spans));
        }
    }

    lines.push(plan_rule(width, gutter, rule_style));

    let footer = match mode {
        Mode::Plan => "proposal · reply with changes, or tab to switch",
        Mode::Coding => "proposal · reply to act on it",
    };
    let footer = elide(footer, width.saturating_sub(2).max(1));

    lines.push(Line::from(vec![
        Span::styled("▏", gutter),
        Span::styled(format!(" {footer}"), dim),
    ]));

    let bands = vec![band; lines.len()];

    EntryLayout {
        lines,
        bands,
        gutter: 1,
    }
}

fn plan_rule(width: usize, gutter: Style, rule: Style) -> Line<'static> {
    let fill = width.saturating_sub(1);

    Line::from(vec![
        Span::styled("▏", gutter),
        Span::styled("─".repeat(fill), rule),
    ])
}

/// A user-initiated switch reads as a divider: `──── plan → coding ────`.
fn mode_change_entry(from: Mode, to: Mode, width: usize, theme: &Theme) -> EntryLayout {
    let mode_color = |mode: Mode| match mode {
        Mode::Plan => theme.warning,
        Mode::Coding => theme.accent,
    };
    let arrow = " → ";
    let transition = format!("{}{arrow}{}", from.label(), to.label());
    let dim = Style::default().fg(theme.dim);

    if width < transition.chars().count() + 2 {
        return EntryLayout {
            lines: vec![Line::from(Span::styled(elide(&transition, width), dim))],
            bands: vec![None],
            gutter: 0,
        };
    }

    let remaining = width - transition.chars().count();
    let left = (remaining - 2) / 2;
    let right = remaining - 2 - left;
    let line = Line::from(vec![
        Span::styled("─".repeat(left), dim),
        Span::raw(" "),
        Span::styled(
            from.label().to_owned(),
            Style::default().fg(mode_color(from)),
        ),
        Span::styled(arrow, dim),
        Span::styled(to.label().to_owned(), Style::default().fg(mode_color(to))),
        Span::raw(" "),
        Span::styled("─".repeat(right), dim),
    ]);

    EntryLayout {
        lines: vec![line],
        bands: vec![None],
        gutter: 0,
    }
}

struct ToolLayout<'a> {
    name: &'a str,
    status: &'a str,
    content: &'a str,
    title: &'a str,
    preview: &'a ToolPreview,
    duration: Option<Duration>,
    width: usize,
    theme: &'a Theme,
    expanded: bool,
    /// Wrapping of a growing body, reused across frames while the tool runs.
    cache: Option<&'a mut WrapCache>,
}

/// Lay out image captions and the cell rows their previews occupy.
fn images_layout(
    images: &[t8ful_core::content::ImageInput],
    width: usize,
    theme: &Theme,
    previews: &mut ImagePreviews,
) -> (EntryLayout, Vec<(usize, PreviewKey, u16, u16)>) {
    let dim = Style::default().fg(theme.dim);
    let mut lines = Vec::new();
    let mut bands = Vec::new();
    let mut placements = Vec::new();
    let max_columns = width
        .saturating_sub(2)
        .clamp(1, crate::images::MAX_PREVIEW_COLUMNS as usize) as u16;

    for (index, image) in images.iter().enumerate() {
        let name = image
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| image.path.display().to_string());
        let caption = format!(
            " [{}] {} · {}x{} · {} · {name}",
            index + 1,
            crate::images::media_label(&image.media_type),
            image.width,
            image.height,
            crate::images::human_bytes(image.bytes),
        );

        lines.push(Line::from(Span::styled(elide(&caption, width.max(1)), dim)));
        bands.push(None);

        let (columns, rows) = crate::images::fit_preview(
            image.width,
            image.height,
            previews.font_size(),
            max_columns,
            crate::images::MAX_PREVIEW_ROWS,
        );
        let key = previews.request(&image.path, columns, rows);
        let placeholder = match previews.outcome(&key) {
            Some(PreviewOutcome::Ready { .. }) => None,
            Some(PreviewOutcome::Failed(message)) => Some(message.clone()),
            None => Some("decoding preview…".to_owned()),
        };
        let offset = lines.len();

        for row in 0..rows {
            let text = if row == 0 {
                placeholder.as_deref().unwrap_or("")
            } else {
                ""
            };

            lines.push(Line::from(Span::styled(text.to_owned(), dim)));
            bands.push(None);
        }

        if placeholder.is_none() {
            placements.push((offset, key, columns, rows));
        }
    }

    (
        EntryLayout {
            lines,
            bands,
            gutter: 1,
        },
        placements,
    )
}

/// Caption-only fallback for an image block outside the transcript layout.
fn images_entry(
    images: &[t8ful_core::content::ImageInput],
    width: usize,
    theme: &Theme,
) -> EntryLayout {
    let mut lines = Vec::new();

    for (index, image) in images.iter().enumerate() {
        let caption = format!(
            " [{}] {} {}x{} · {} · {}",
            index + 1,
            crate::images::media_label(&image.media_type),
            image.width,
            image.height,
            crate::images::human_bytes(image.bytes),
            image
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| image.path.display().to_string())
        );

        lines.push(Line::from(Span::styled(
            elide(&caption, width.max(1)),
            Style::default().fg(theme.dim),
        )));
    }

    EntryLayout {
        bands: vec![None; lines.len()],
        lines,
        gutter: 1,
    }
}

fn build_tool(layout: ToolLayout<'_>) -> EntryLayout {
    let ToolLayout {
        name,
        status,
        content,
        title,
        preview,
        duration,
        width,
        theme,
        expanded,
        cache,
    } = layout;

    let band = theme.tool_bg;
    let bare = band.is_none();
    let lead = if bare { "▏" } else { " " };
    let lead_style = if bare {
        Style::default().fg(theme.tool)
    } else {
        Style::default()
    };
    // A shell result can be `ok` at the tool layer and still report a failure
    // in its first line; that card keeps the error glyph and never collapses.
    let failed = content_reports_failure(status, content);
    let glyph = if failed { "✗" } else { tool_glyph(status) };
    let glyph_style = Style::default().fg(if failed {
        theme.error
    } else {
        tool_status_color(status, theme)
    });
    let dim = Style::default().fg(theme.dim);
    let label_style = Style::default().fg(theme.fg);
    let right = duration
        .map(|duration| format!("· {}", format_tool_duration(duration)))
        .unwrap_or_default();
    let reserved = if right.is_empty() {
        0
    } else {
        right.chars().count() + 1
    };
    let display_name = friendly_tool_name(name);
    let label = if title.is_empty() {
        display_name.into_owned()
    } else {
        format!("{display_name} {title}")
    };
    let label_width = width.saturating_sub(3 + reserved).max(1);
    let chunks = wrap_ranges(&label, label_width);
    let mut lines = Vec::new();
    let mut bands = Vec::new();

    for (index, (start, end)) in chunks.iter().enumerate() {
        let text = &label[*start..*end];
        let mut spans = vec![Span::styled(lead, lead_style)];

        if index == 0 {
            spans.push(Span::styled(glyph, glyph_style));
            spans.push(Span::styled(format!(" {text}"), label_style));
        } else {
            // Continuation lines indent under the label, past the glyph.
            spans.push(Span::styled(format!("   {text}"), label_style));
        }

        if index + 1 == chunks.len() && !right.is_empty() {
            let used = 3 + text.chars().count();
            let padding = width.saturating_sub(used + right.chars().count() + 1);

            spans.push(Span::styled(" ".repeat(padding), dim));
            spans.push(Span::styled(right.clone(), dim));
        }

        lines.push(Line::from(spans));
        bands.push(band);
    }

    let body_width = width.saturating_sub(1).max(1);
    let collapsible = !failed && matches!(status, "ok" | "running");
    let running = status == "running";

    if !preview.is_empty() {
        let subtitle = tool_subtitle(name, content);
        let subtitle_shown = !subtitle.is_empty();

        if subtitle_shown {
            let subtitle = elide(&subtitle, body_width);

            lines.push(Line::from(vec![
                Span::styled(lead, lead_style),
                Span::styled(format!(" {subtitle}"), dim),
            ]));
            bands.push(band);
        }

        let preview_lines = preview_lines(preview, body_width, theme);
        let total = preview_lines.len();
        let collapsed = collapsible && !expanded && total > PREVIEW_COLLAPSED_LINES;
        let shown = if collapsed {
            PREVIEW_COLLAPSED_LINES
        } else {
            total
        };

        for line in preview_lines.into_iter().take(shown) {
            let mut spans = vec![Span::styled(lead, lead_style)];

            spans.extend(line.spans);
            lines.push(Line::from(spans));
            bands.push(band);
        }

        if collapsed {
            let marker = format!("… ({} more lines, ctrl+o to expand)", total - shown);
            let marker = elide(&marker, width.saturating_sub(2).max(1));

            lines.push(Line::from(vec![
                Span::styled(lead, lead_style),
                Span::styled(format!(" {marker}"), dim),
            ]));
            bands.push(band);
        } else if !collapsible && subtitle_shown {
            // Keep every error line visible, not only the summary subtitle.
            for (start, end) in wrap_ranges(content, body_width).into_iter().skip(1) {
                lines.push(Line::from(vec![
                    Span::styled(lead, lead_style),
                    Span::styled(content[start..end].to_owned(), dim),
                ]));
                bands.push(band);
            }
        }

        return EntryLayout {
            lines,
            bands,
            gutter: 1,
        };
    }

    // A running card with no output yet stays a single header line.
    if running && content.is_empty() {
        return EntryLayout {
            lines,
            bands,
            gutter: 1,
        };
    }

    // A collapsed card shows only its tail, so a running command never pays to
    // materialize the output it is about to hide.
    let tail = if collapsible && !expanded {
        Some(COLLAPSED_TOOL_LINES)
    } else {
        None
    };
    let (total, rows) = match cache {
        Some(cache) => {
            cache.set_width(body_width);
            cache.page(content, tail)
        }
        None => {
            let ranges = wrap_ranges(content, body_width);
            let total = ranges.len();
            let skip = tail.map_or(0, |tail| total.saturating_sub(tail));

            (total, ranges[skip..].to_vec())
        }
    };
    let collapsed = collapsible && !expanded && total > COLLAPSED_TOOL_LINES;
    let hidden = total.saturating_sub(rows.len());

    if collapsed {
        let marker = format!("… ({hidden} earlier lines, ctrl+o to expand)");
        let marker = elide(&marker, width.saturating_sub(1).max(1));

        lines.push(Line::from(vec![
            Span::styled(lead, lead_style),
            Span::styled(marker, dim),
        ]));
        bands.push(band);
    }

    for (start, end) in rows {
        lines.push(Line::from(vec![
            Span::styled(lead, lead_style),
            Span::styled(content[start..end].to_owned(), dim),
        ]));
        bands.push(band);
    }

    EntryLayout {
        lines,
        bands,
        gutter: 1,
    }
}

/// Whether the expand toggle can change how this entry renders.
///
/// A card whose body cannot fill more than the collapsed window looks the same
/// either way, so rebuilding it on ctrl-o would be wasted work. The collapse
/// decision itself is made on wrapped rows (see [`build_tool`]), so this test
/// has to count the same rows: one long line can wrap past the window even when
/// its source has a single line, and a gate that counted source lines would
/// leave such a card stuck behind a `ctrl+o` marker it never honours.
fn expanded_affects(entry: &Entry, width: usize) -> bool {
    let Entry::Tool {
        status,
        content,
        preview,
        ..
    } = entry
    else {
        return false;
    };

    if content_reports_failure(status, content) || !matches!(status.as_str(), "ok" | "running") {
        return false;
    }

    let body_width = width.saturating_sub(1).max(1);

    if preview.is_empty() {
        return wraps_past(content, body_width, COLLAPSED_TOOL_LINES);
    }

    // A preview is rendered by kind-specific code, so its exact row count is not
    // the plain wrapping of its text. Over-approximating is safe here: a
    // needless rebuild only costs one wrap of that card.
    preview.text.lines().count() > PREVIEW_COLLAPSED_LINES
        || wraps_past(&preview.text, body_width, PREVIEW_COLLAPSED_LINES)
}

/// Whether `content` wraps to more than `limit` rows at `width`.
///
/// Counts rows exactly as [`wrap_ranges`] would, but stops at the first row past
/// the limit and allocates nothing, because only the answer matters here.
fn wraps_past(content: &str, width: usize, limit: usize) -> bool {
    let width = width.max(1);
    let mut rows = 0;

    for segment in content.split('\n') {
        rows += segment.chars().count().max(1).div_ceil(width);

        if rows > limit {
            return true;
        }
    }

    false
}

/// Whether a tool-level success actually reports a failed command.
fn content_reports_failure(status: &str, content: &str) -> bool {
    if status != "ok" {
        return false;
    }

    let Some(first) = content.lines().next() else {
        return false;
    };
    let first = first.trim();

    if first.starts_with("[timed out")
        || first.starts_with("[cancelled")
        || first.starts_with("[terminated by signal")
    {
        return true;
    }

    if let Some(rest) = first.strip_prefix("[exit code ") {
        let code = rest.split(',').next().unwrap_or_default().trim();

        return code.parse::<i32>().is_ok_and(|code| code != 0);
    }

    false
}

fn tool_subtitle(name: &str, content: &str) -> String {
    match name {
        "apply_patch" | "edit_file" | "write_file" => {
            content.lines().next().unwrap_or_default().trim().to_owned()
        }
        _ => String::new(),
    }
}

fn preview_lines(preview: &ToolPreview, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    match preview.kind {
        PreviewKind::Diff => diff_preview_lines(&preview.text, width, theme),
        PreviewKind::Code => crate::markdown::highlight_preview(
            &preview.text,
            preview.language.as_deref(),
            width,
            theme,
        )
        .into_iter()
        .map(Line::from)
        .collect(),
        PreviewKind::None => Vec::new(),
    }
}

fn diff_preview_lines(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for line in text.lines() {
        let style = if line.starts_with('+') {
            Style::default().fg(theme.success)
        } else if line.starts_with('-') {
            Style::default().fg(theme.error)
        } else if line.starts_with("@@") || line.starts_with("diff ") {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.dim)
        };

        if line.is_empty() {
            lines.push(Line::from(Span::styled(String::new(), style)));

            continue;
        }

        for (start, end) in wrap_ranges(line, width) {
            lines.push(Line::from(Span::styled(line[start..end].to_owned(), style)));
        }
    }

    lines
}

fn tool_glyph(status: &str) -> &'static str {
    match status {
        "ok" => "✓",
        "running" => "…",
        "error" => "✗",
        "denied" => "⊘",
        "input required" => "…",
        _ => "?",
    }
}

fn tool_status_color(status: &str, theme: &Theme) -> Color {
    match status {
        "ok" => theme.success,
        "running" => theme.accent,
        "error" => theme.error,
        "denied" => theme.warning,
        "input required" => theme.accent,
        _ => theme.dim,
    }
}

fn format_tool_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();

    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else if seconds < 60.0 {
        format!("{}s", seconds.round() as u64)
    } else {
        crate::spinner::format_elapsed(duration)
    }
}

fn busy_line(state: &AppState, width: usize) -> Line<'static> {
    let elapsed = state
        .turn_started
        .map(|started| started.elapsed())
        .unwrap_or_default();
    let frame = crate::spinner::frame(state.spinner_phase);
    let verb = crate::spinner::verb(state.turn_seed, crate::spinner::verb_bucket(elapsed));
    let label = format!("{frame} {verb}…");
    let elapsed_text = crate::spinner::format_elapsed(elapsed);
    let wide = format!(" {label} · {elapsed_text}");
    let medium = format!(" {label}");
    let dim = Style::default().fg(state.theme.dim);
    let accent = Style::default().fg(state.theme.accent);

    // Shed the elapsed time first, then shorten the label. The leading space is
    // part of the line, so the content never overflows the transcript width.
    if wide.chars().count() <= width {
        return Line::from(vec![
            Span::styled(format!(" {frame}"), dim),
            Span::styled(format!(" {verb}…"), accent),
            Span::styled(format!(" · {elapsed_text}"), dim),
        ]);
    }

    if medium.chars().count() <= width {
        return Line::from(vec![
            Span::styled(format!(" {frame}"), dim),
            Span::styled(format!(" {verb}…"), accent),
        ]);
    }

    let available = width.saturating_sub(1);
    let mut kept = String::new();

    for character in label.chars().take(available) {
        kept.push(character);
    }

    Line::from(vec![Span::styled(" ", dim), Span::styled(kept, accent)])
}

/// Wrap text to `width` characters, preserving explicit newlines.
///
/// Returns byte ranges into the original text, so unchanged entries do not
/// duplicate their content in the layout cache.
pub fn wrap_ranges(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut ranges = Vec::new();

    let start = wrap_completed_lines(text, width, 0, &mut ranges);

    ranges.push((start, text.len()));

    ranges
}

/// Start of the line still being written, which is the text after the last
/// newline.
fn last_line_start(text: &str) -> usize {
    text.rfind('\n').map(|index| index + 1).unwrap_or(0)
}

/// Push the ranges of every finished line, without the unfinished last one.
///
/// `base` is added to each offset so a suffix of a larger body can be wrapped
/// into the same coordinate space. Returns the start of the unfinished line.
fn wrap_completed_lines(
    text: &str,
    width: usize,
    base: usize,
    ranges: &mut Vec<(usize, usize)>,
) -> usize {
    let width = width.max(1);
    let mut line_start = 0;
    let mut count = 0;

    for (offset, character) in text.char_indices() {
        if character == '\n' {
            ranges.push((base + line_start, base + offset));
            line_start = offset + 1;
            count = 0;
        } else {
            if count == width {
                ranges.push((base + line_start, base + offset));
                line_start = offset;
                count = 0;
            }

            count += 1;
        }
    }

    base + line_start
}

/// Wrapping of a tool body that grows while its command runs.
///
/// A finished line never changes its wrapping, so only the bytes after the last
/// newline are wrapped again when the next chunk arrives. The cache is only
/// valid for append-only content; shrinking content resets it.
#[derive(Debug, Default)]
pub struct WrapCache {
    width: usize,
    /// Byte offset just past the last newline covered by `ranges`.
    stable_len: usize,
    ranges: Vec<(usize, usize)>,
}

impl WrapCache {
    pub fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            stable_len: 0,
            ranges: Vec::new(),
        }
    }

    /// Point the cache at a new width, dropping stale ranges.
    pub fn set_width(&mut self, width: usize) {
        let width = width.max(1);

        if self.width != width {
            self.width = width;
            self.stable_len = 0;
            self.ranges.clear();
        }
    }

    /// Extend the cache to cover `content`, then return the total row count and
    /// the rows to draw.
    ///
    /// `tail` limits the returned rows to the last `tail` rows of the body,
    /// which is what a collapsed card shows; the count is always complete so the
    /// marker can report the hidden lines.
    pub fn page(&mut self, content: &str, tail: Option<usize>) -> (usize, Vec<(usize, usize)>) {
        self.sync(content);

        let tail_ranges = wrap_ranges(&content[self.stable_len..], self.width);
        let total = self.ranges.len() + tail_ranges.len();
        let skip = tail.map_or(0, |tail| total.saturating_sub(tail));
        let mut rows = Vec::with_capacity(total - skip);

        if skip < self.ranges.len() {
            rows.extend_from_slice(&self.ranges[skip..]);
        }

        let first_tail = skip.saturating_sub(self.ranges.len());

        rows.extend(
            tail_ranges[first_tail..]
                .iter()
                .map(|(start, end)| (start + self.stable_len, end + self.stable_len)),
        );

        (total, rows)
    }

    /// Wrap the newly finished lines of `content` into the cache.
    fn sync(&mut self, content: &str) {
        if content.len() < self.stable_len {
            self.stable_len = 0;
            self.ranges.clear();
        }

        let stable = last_line_start(content);

        if stable <= self.stable_len {
            return;
        }

        let start = self.stable_len;

        wrap_completed_lines(&content[start..stable], self.width, start, &mut self.ranges);
        self.stable_len = stable;
    }
}

/// Row and column of the composer cursor inside the prompt box.
///
/// Wrapping is hard: the `width`-th character moves the cursor to the next
/// row, matching [`composer_lines`].
fn cursor_offset_at(input: &str, cursor: usize, inner_width: usize) -> (usize, usize) {
    let width = inner_width.max(1);
    let mut row = 0;
    let mut column = 0;

    for (index, character) in input.chars().enumerate() {
        if index == cursor {
            break;
        }

        if character == '\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;

            if column == width {
                row += 1;
                column = 0;
            }
        }
    }

    (row, column)
}

/// Row and column of the cursor when it sits at the end of the text.
#[cfg(test)]
fn cursor_offset(input: &str, inner_width: usize) -> (usize, usize) {
    cursor_offset_at(input, input.chars().count(), inner_width)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use std::time::Instant;

    use super::{
        ToolLayout, TranscriptLayout, WrapCache, attachment_tray_lines, build_tool, busy_line,
        composer_layout, composer_lines, composer_rows, cursor_offset, cursor_offset_at,
        images_layout, wrap_ranges,
    };
    use crate::state::ToolPreview;
    use crate::state::{AppState, CardPlacement, Entry, Header};

    fn ranges(text: &str, width: usize) -> Vec<&str> {
        wrap_ranges(text, width)
            .into_iter()
            .map(|(start, end)| &text[start..end])
            .collect()
    }

    fn state(entries: Vec<Entry>) -> AppState {
        let mut state = AppState::new(Header {
            model: "vendor/model".to_owned(),
            mode: t8ful_core::policy::Mode::Coding,
            session_id: "session-1".to_owned(),
            project_root: PathBuf::from("/work/project"),
            launch_dir: PathBuf::from("/work/project"),
            version: "0.1.0".to_owned(),
            branch: None,
        });

        state.entries = entries;

        state
    }

    #[test]
    fn wraps_long_text_at_the_requested_width() {
        assert_eq!(ranges("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(ranges("ab\n\ncd", 4), vec!["ab", "", "cd"]);
    }

    #[test]
    fn wrapping_preserves_exact_widths_trailing_newlines_and_unicode() {
        assert_eq!(ranges("abcd", 4), vec!["abcd"]);
        assert_eq!(ranges("abc\n", 4), vec!["abc", ""]);
        assert_eq!(
            ranges("", 4),
            vec![""],
            "an empty body still occupies one line"
        );
        assert_eq!(ranges("héllo→", 2), vec!["hé", "ll", "o→"]);
    }

    #[test]
    fn wrap_cache_append_matches_a_fresh_wrap() {
        let corpus = [
            "one\ntwo\nthree",
            "one\r\ntwo\r\n",
            "héllo→\nwörld\n",
            "abcdefghijklmnopqrstuvwxyz\nshort\n\nlong line here",
            "no newline at all",
            "",
        ];

        for text in corpus {
            for width in [1, 2, 7, 20] {
                let mut cache = WrapCache::new(width);

                for end in 1..=text.len() {
                    if !text.is_char_boundary(end) {
                        continue;
                    }

                    let prefix = &text[..end];
                    let expected = wrap_ranges(prefix, width);
                    let (total, rows) = cache.page(prefix, None);

                    assert_eq!(total, expected.len(), "{text:?} w{width} @{end}");
                    assert_eq!(rows, expected, "{text:?} w{width} @{end}");
                }

                let fresh = wrap_ranges(text, width);
                let (total, tail) = cache.page(text, Some(2));

                assert_eq!(total, fresh.len(), "{text:?} w{width}");
                assert_eq!(
                    tail,
                    fresh[fresh.len().saturating_sub(2)..].to_vec(),
                    "{text:?} w{width}"
                );
            }
        }
    }

    #[test]
    fn wrap_cache_resets_when_the_width_changes() {
        let text = "abcdefgh\nijklmnop";

        let mut cache = WrapCache::new(4);
        cache.page(text, None);
        cache.set_width(3);

        let expected = wrap_ranges(text, 3);
        let (total, rows) = cache.page(text, None);

        assert_eq!((total, rows), (expected.len(), expected));
    }

    #[test]
    fn a_running_tool_card_built_incrementally_matches_a_fresh_build() {
        let state = state(Vec::new());
        let body = "line one\nline two is longer than the card width\n\r\nsecond para\n                    héllo wörld with a wide tail\n";

        for width in [20, 41] {
            for expanded in [false, true] {
                let expected = build_tool(ToolLayout {
                    name: "bash",
                    status: "running",
                    content: body,
                    title: "",
                    preview: &ToolPreview::default(),
                    duration: None,
                    width,
                    theme: &state.theme,
                    expanded,
                    cache: None,
                });
                let mut cache = WrapCache::default();
                let mut built = None;

                for end in 1..=body.len() {
                    if !body.is_char_boundary(end) {
                        continue;
                    }

                    built = Some(build_tool(ToolLayout {
                        name: "bash",
                        status: "running",
                        content: &body[..end],
                        title: "",
                        preview: &ToolPreview::default(),
                        duration: None,
                        width,
                        theme: &state.theme,
                        expanded,
                        cache: Some(&mut cache),
                    }));
                }

                let built = built.expect("a built card");

                assert_eq!(built.lines, expected.lines, "w{width} expanded {expanded}");
                assert_eq!(built.bands, expected.bands, "w{width} expanded {expanded}");
                assert_eq!(built.gutter, expected.gutter);
            }
        }
    }

    #[test]
    fn the_layout_cache_reuses_unchanged_entries_and_invalidates_on_width_theme_and_history() {
        let mut state = state(vec![
            Entry::User("hello".to_owned()),
            Entry::Reasoning("a thought".to_owned()),
            tool("one\ntwo\nthree\nfour\nfive\nsix", "ok"),
            tool("one\ntwo", "ok"),
            Entry::Assistant("answer".to_owned()),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);
        let builds = layout.entry_builds;

        layout.sync(&state, 40);

        assert_eq!(layout.entry_builds, builds);
        assert_eq!(layout.entries.len(), 5);

        state.entries.push(Entry::Info("note".to_owned()));
        layout.sync(&state, 40);

        assert_eq!(layout.entry_builds, builds + 1);

        let before = plain(&state, &layout);
        let builds = layout.entry_builds;

        // The six-line card gains from expanding; the two-line card fits
        // either way, so only one entry is rebuilt.
        state.toggle_expanded();
        layout.sync(&state, 40);

        assert_eq!(
            layout.entry_builds,
            builds + 1,
            "ctrl-o rebuilds the six-line tool card only"
        );

        state.toggle_reasoning();
        layout.sync(&state, 40);

        assert_eq!(
            layout.entry_builds,
            builds + 2,
            "ctrl-r rebuilds only the reasoning entry"
        );

        state.toggle_expanded();
        state.toggle_reasoning();
        layout.sync(&state, 40);

        assert_eq!(
            plain(&state, &layout),
            before,
            "toggling back reproduces the original rendering"
        );

        // Width and theme changes rebuild every entry.
        let builds = layout.entry_builds;

        layout.sync(&state, 20);

        assert_eq!(layout.entry_builds, builds + 6);

        let builds = layout.entry_builds;

        state.theme = crate::theme::Theme::ansi();
        layout.sync(&state, 20);

        assert_eq!(layout.entry_builds, builds + 6);

        // Restored history replaces the cache instead of reusing stale entries.
        state.apply_event(t8ful_core::event::AgentEvent::HistoryRestored {
            entries: Vec::new(),
        });
        state.entries = vec![Entry::Info("restored".to_owned())];
        layout.sync(&state, 20);

        assert_eq!(layout.entry_builds, builds + 7);

        let restored = plain(&state, &layout);

        assert_eq!(restored.len(), 1);
        assert!(restored[0].contains("restored"), "{restored:?}");
    }

    #[test]
    fn streaming_updates_only_rebuild_the_streaming_layout() {
        let mut state = state(vec![Entry::User("cached entry".to_owned())]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);
        let builds = layout.entry_builds;

        state.apply_event(t8ful_core::event::AgentEvent::TextDelta {
            text: "hel".to_owned(),
        });
        layout.sync(&state, 40);

        assert_eq!(layout.entry_builds, builds);
        // One cached entry, one separator line, one streaming line.
        assert_eq!(layout.total_lines(), 3);

        state.apply_event(t8ful_core::event::AgentEvent::TextDelta {
            text: "lo".to_owned(),
        });
        layout.sync(&state, 40);

        assert_eq!(layout.entry_builds, builds);

        let lines = layout.visible_lines(&state, 2, 3);

        assert!(
            lines[0].0.to_string().contains("hello"),
            "{:?}",
            lines[0].0.to_string()
        );
    }

    #[test]
    fn visible_lines_cross_entry_boundaries() {
        let state = state(vec![
            Entry::User("first\nsecond".to_owned()),
            Entry::Assistant("third".to_owned()),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 80);

        // Two lines of user text, one separator line, one assistant line.
        assert_eq!(layout.total_lines(), 4);

        let lines = layout.visible_lines(&state, 1, 4);
        let text = lines
            .iter()
            .map(|(line, _)| line.to_string())
            .collect::<Vec<_>>();

        assert!(text[0].contains("second"), "{text:?}");
        assert_eq!(text[1], "", "a blank separator precedes the next block");
        assert!(text[2].contains("third"), "{text:?}");
    }

    #[test]
    fn consecutive_tool_cards_are_separated_by_exactly_one_blank_row() {
        let state = state(vec![
            tool("first result", "ok"),
            tool("second result", "ok"),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 60);

        let lines = plain(&state, &layout);
        let gaps = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.is_empty())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), layout.total_lines());
        assert!(!lines[0].is_empty(), "no gap before the first block");
        assert!(!lines[lines.len() - 1].is_empty(), "no gap at the end");
        assert_eq!(gaps.len(), 1, "{lines:#?}");
        assert!(!lines[gaps[0] - 1].trim().is_empty(), "{lines:#?}");
        assert!(
            lines[gaps[0] + 1].contains("Read"),
            "the next block starts right after the gap: {lines:#?}"
        );
    }

    #[test]
    fn a_hidden_entry_never_introduces_or_leaves_a_gap() {
        let mut state = state(vec![
            Entry::User("hello".to_owned()),
            Entry::Reasoning("hidden".to_owned()),
            Entry::Assistant("world".to_owned()),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);

        let lines = plain(&state, &layout);

        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert!(lines[0].contains("hello"), "{lines:#?}");
        assert_eq!(lines[1], "");
        assert!(lines[2].contains("world"), "{lines:#?}");

        state.toggle_reasoning();
        layout.sync(&state, 40);

        let lines = plain(&state, &layout);

        assert!(
            lines.iter().any(|line| line.contains("hidden")),
            "{lines:#?}"
        );
        assert_eq!(
            lines.iter().filter(|line| line.is_empty()).count(),
            2,
            "one gap on each side of the reasoning block: {lines:#?}"
        );
    }

    #[test]
    fn composer_wrapping_is_linear_and_agrees_with_the_cursor_row() {
        let input = "x".repeat(8_000);
        let wrapped = composer_lines(&input, 20);

        assert_eq!(wrapped.len(), 400);
        assert!(wrapped.iter().all(|line| line.chars().count() == 20));

        let layout = composer_layout(&input, input.chars().count(), 20);

        // A full last line leaves the cursor on the row after it, where the
        // next character would go; the composer adds that row automatically.
        assert_eq!(layout.row, 400);
        assert_eq!(layout.column, 0);
        assert_eq!(layout.height(), 6);
    }

    #[test]
    fn composer_wrapping_counts_characters_not_bytes_and_breaks_at_the_width() {
        assert_eq!(composer_lines("ééé\nab", 2), vec!["éé", "é", "ab"]);
        assert_eq!(composer_lines("abcd", 2), vec!["ab", "cd"]);
        assert_eq!(composer_lines("ab", 2), vec!["ab"]);
        assert_eq!(composer_lines("a\n", 10), vec!["a", ""]);

        let layout = composer_layout("héllo", 2, 2);

        assert_eq!((layout.row, layout.column), (1, 0));
        assert_eq!(layout.lines, vec!["hé", "ll", "o"]);
    }

    #[test]
    fn the_composer_grows_with_wrapped_input_and_caps_at_six_rows() {
        assert_eq!(composer_rows("", 0, 20), 1);
        assert_eq!(composer_rows("abc", 3, 20), 1);
        assert_eq!(composer_rows("a\nb", 3, 20), 2);
        assert_eq!(composer_rows(&"x".repeat(45), 45, 20), 3);
        assert_eq!(composer_rows(&"x".repeat(500), 500, 20), 6);
        // Editing in the middle sizes the composer from the cursor row.
        assert_eq!(composer_rows("a\nb\nc", 1, 20), 1);
        assert_eq!(composer_rows("a\nb\nc", 0, 20), 1);
    }

    #[test]
    fn image_entries_show_a_caption_and_reserve_preview_rows() {
        let image = t8ful_core::content::ImageInput::new(
            "image/png",
            PathBuf::from("/state/images/one.png"),
            640,
            480,
            1536,
        );
        let mut previews = crate::images::ImagePreviews::new();
        let (layout, placements) = images_layout(
            std::slice::from_ref(&image),
            70,
            &crate::theme::Theme::pi(),
            &mut previews,
        );
        let text = layout
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            text.contains("[1] PNG · 640x480 · 1.5 KiB · one.png"),
            "{text}"
        );
        assert!(text.contains("decoding preview…"), "{text}");
        assert!(placements.is_empty(), "a pending preview is not renderable");

        let max_columns = 70_usize
            .saturating_sub(2)
            .clamp(1, crate::images::MAX_PREVIEW_COLUMNS as usize) as u16;
        let (_, rows) = crate::images::fit_preview(
            640,
            480,
            previews.font_size(),
            max_columns,
            crate::images::MAX_PREVIEW_ROWS,
        );

        assert_eq!(layout.lines.len(), 1 + rows as usize);

        let mut state = state(vec![Entry::Images(vec![image])]);

        state
            .pending_attachments
            .push(t8ful_core::content::ImageInput::new(
                "image/gif",
                PathBuf::from("/state/images/two.gif"),
                32,
                32,
                512,
            ));

        let tray = attachment_tray_lines(&state, 80)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(tray.contains("1 GIF 32x32 512 B two.gif"), "{tray}");
    }

    #[test]
    fn the_busy_line_shows_a_verb_then_sheds_detail_when_narrow() {
        let mut state = state(Vec::new());

        state.busy = true;
        state.status = "running Read".to_owned();
        state.turn_started = Some(Instant::now());
        state.turn_seed = crate::spinner::turn_seed(1, "session-1");

        let wide = busy_line(&state, 80).to_string();

        assert!(wide.contains("|"), "{wide}");
        assert!(wide.contains('…'), "{wide}");
        assert!(wide.contains("0s"), "{wide}");
        assert!(!wide.contains("running Read"), "{wide}");

        let medium = busy_line(&state, 16).to_string();

        assert!(medium.contains('…'), "{medium}");
        assert!(!medium.contains("0s"), "{medium}");
        assert!(medium.chars().count() <= 16, "{medium}");

        let narrow = busy_line(&state, 8).to_string();

        assert!(narrow.chars().count() <= 8, "{narrow:?}");
        assert!(!narrow.contains("0s"), "{narrow:?}");
    }

    fn tool(body: &str, status: &str) -> Entry {
        Entry::Tool {
            call_id: "call-1".to_owned(),
            name: "read_file".to_owned(),
            status: status.to_owned(),
            content: body.to_owned(),
            truncated: false,
            title: "src/lib.rs".to_owned(),
            preview: ToolPreview::default(),
            duration: None,
            placement: CardPlacement::default(),
        }
    }

    fn plain(state: &AppState, layout: &TranscriptLayout) -> Vec<String> {
        layout
            .visible_lines(state, 0, layout.total_lines())
            .into_iter()
            .map(|(line, _)| line.to_string())
            .collect()
    }

    #[test]
    fn streamed_markdown_renders_like_the_committed_entry() {
        let doc = "# Title\n\nA paragraph with `code`, **bold**, and a [link](https://x.test).\n\n\
- one\n- two\n  - nested\n\n> quoted \n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n\
héllo → wörld supercalifragilisticexpialidocious\n";
        let mut state = state(Vec::new());
        let mut layout = TranscriptLayout::default();
        let mut sent = 0;

        for end in 0..=doc.len() {
            if !doc.is_char_boundary(end) {
                continue;
            }

            state.apply_event(t8ful_core::event::AgentEvent::TextDelta {
                text: doc[sent..end].to_owned(),
            });
            sent = end;
            layout.sync(&state, 40);
        }

        let streamed = plain(&state, &layout);

        state.flush_streaming();
        layout.sync(&state, 40);

        assert_eq!(streamed, plain(&state, &layout));
        assert!(
            streamed.iter().any(|line| line.contains("Title")),
            "{streamed:?}"
        );
    }

    #[test]
    fn streamed_reasoning_renders_like_the_committed_entry() {
        let doc = "## plan\n\n- **bold** step with `code`\n\nmore text here\n";
        let mut state = state(Vec::new());
        let mut layout = TranscriptLayout::default();

        state.toggle_reasoning();

        let mut sent = 0;

        for end in 0..=doc.len() {
            if !doc.is_char_boundary(end) {
                continue;
            }

            state.apply_event(t8ful_core::event::AgentEvent::ReasoningDelta {
                text: doc[sent..end].to_owned(),
            });
            sent = end;
            layout.sync(&state, 40);
        }

        let streamed = plain(&state, &layout);
        let bands = layout.visible_lines(&state, 0, layout.total_lines());

        state.commit_reasoning();
        layout.sync(&state, 40);

        assert_eq!(streamed, plain(&state, &layout));

        for (line, _) in &bands {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(state.theme.dim), "{span:?}");
            }
        }
    }

    #[test]
    fn collapsed_tool_output_keeps_the_tail_and_reports_hidden_lines() {
        let body = (1..=10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = state(vec![tool(&body, "ok")]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);

        let text = plain(&state, &layout).join("\n");

        assert!(text.contains("✓ Read src/lib.rs"), "{text}");
        assert!(text.contains("6 earlier lines, ctrl+o to expand"), "{text}");
        assert!(!text.contains("line 1\n"), "{text}");
        assert!(text.contains("line 10"), "{text}");
        assert_eq!(layout.total_lines(), 1 + 1 + 4);

        state.toggle_expanded();
        layout.sync(&state, 40);

        assert_eq!(layout.total_lines(), 1 + 10);
        assert!(plain(&state, &layout).join("\n").contains("line 1"));
    }

    #[test]
    fn a_skill_card_collapses_to_its_tail_and_expands() {
        let body = (1..=10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let entry = Entry::Tool {
            call_id: "skill:review".to_owned(),
            name: "skill".to_owned(),
            status: "ok".to_owned(),
            content: body,
            truncated: false,
            title: "review".to_owned(),
            preview: ToolPreview::default(),
            duration: None,
            placement: CardPlacement::default(),
        };
        let mut state = state(vec![entry]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);

        let text = plain(&state, &layout).join("\n");

        assert!(text.contains("✓ Skill review"), "{text}");
        assert!(text.contains("6 earlier lines, ctrl+o to expand"), "{text}");
        assert!(!text.contains("line 1\n"), "{text}");
        assert!(text.contains("line 10"), "{text}");
        assert_eq!(layout.total_lines(), 1 + 1 + 4);

        state.toggle_expanded();
        layout.sync(&state, 40);

        assert_eq!(layout.total_lines(), 1 + 10);
        assert!(plain(&state, &layout).join("\n").contains("line 1"));
    }

    #[test]
    fn an_mcp_card_names_its_tool_once() {
        let mut state = state(Vec::new());
        let mut layout = TranscriptLayout::default();
        let name = "mcp__exa__web_search_exa";

        state.apply_event(t8ful_core::event::AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: name.to_owned(),
            arguments: serde_json::json!({ "query": "inkwell rust latest version" }),
        });
        state.apply_event(t8ful_core::event::AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: name.to_owned(),
            status: t8ful_core::tool::ToolStatus::Ok,
            content: "- Updated: about 1 month ago".to_owned(),
            truncated: false,
            process_id: None,
        });

        layout.sync(&state, 60);

        let header = plain(&state, &layout).remove(0);

        // `friendly_tool_name` already spells out `Exa · web_search_exa`; the
        // title must not repeat the server's own tool name after it.
        assert!(header.contains("Exa · web_search_exa"), "{header}");
        assert!(
            !header.contains("web_search_exa web_search_exa"),
            "{header}"
        );
    }

    #[test]
    fn a_card_whose_long_lines_wrap_can_still_be_expanded() {
        // Exa answers with a couple of short lines and one long JSON line, so
        // the source has three lines but the card wraps far past its window.
        // The expand toggle must use the renderer's wrapped-row count; a gate
        // that counted source lines left the card stuck behind a marker it
        // could never honour.
        let body = format!(
            "Title: llvm\nURL: https://crates.io/keywords/llvm\n{}",
            "x".repeat(600)
        );
        let mut state = state(vec![Entry::Tool {
            call_id: "call-1".to_owned(),
            name: "mcp__exa__web_fetch_exa".to_owned(),
            status: "ok".to_owned(),
            content: body,
            truncated: false,
            title: String::new(),
            preview: ToolPreview::default(),
            duration: None,
            placement: CardPlacement::default(),
        }]);
        let mut layout = TranscriptLayout::default();

        // Wide enough that the collapse marker is not itself elided, so the
        // assertions below are about the card, not about the marker's width.
        layout.sync(&state, 80);

        let collapsed = plain(&state, &layout).join("\n");

        assert!(
            collapsed.contains("earlier lines, ctrl+o to expand"),
            "{collapsed}"
        );
        assert!(!collapsed.contains("Title: llvm"), "{collapsed}");

        state.toggle_expanded();
        layout.sync(&state, 80);

        let expanded = plain(&state, &layout).join("\n");

        assert!(!expanded.contains("earlier lines"), "{expanded}");
        assert!(expanded.contains("Title: llvm"), "{expanded}");
    }

    #[test]
    fn non_ok_tool_results_are_never_collapsed() {
        let body = (1..=10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        for status in [
            "error",
            "denied",
            "indeterminate",
            "not executed",
            "input required",
        ] {
            let state = state(vec![tool(&body, status)]);
            let mut layout = TranscriptLayout::default();

            layout.sync(&state, 40);

            assert_eq!(layout.total_lines(), 11, "{status}");
            assert!(!plain(&state, &layout).join("\n").contains("earlier lines"));
        }
    }

    #[test]
    fn hidden_reasoning_takes_no_space_and_can_be_toggled() {
        let mut state = state(vec![Entry::Reasoning("private thought".to_owned())]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);

        assert_eq!(layout.total_lines(), 0);
        assert!(plain(&state, &layout).is_empty());

        state.toggle_reasoning();
        layout.sync(&state, 40);

        let lines = layout.visible_lines(&state, 0, layout.total_lines());

        assert_eq!(lines.len(), 1);
        assert!(lines[0].0.to_string().contains("private thought"));
        assert!(
            lines[0].0.spans.iter().any(|span| {
                span.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::ITALIC)
                    && span.style.fg == Some(state.theme.dim)
            }),
            "reasoning should be dim italic"
        );
    }

    #[test]
    fn reasoning_renders_as_markdown_and_stays_dim_italic() {
        let mut state = state(vec![Entry::Reasoning(
            "## plan\n\n- **bold** step with `code`\n".to_owned(),
        )]);
        let mut layout = TranscriptLayout::default();

        state.toggle_reasoning();
        layout.sync(&state, 60);

        let lines = layout.visible_lines(&state, 0, layout.total_lines());
        let text = lines
            .iter()
            .map(|(line, _)| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("plan"), "{text}");
        assert!(!text.contains("##"), "{text}");
        assert!(!text.contains("**"), "{text}");
        assert!(!text.contains('`'), "{text}");
        assert!(text.contains("bold"), "{text}");
        assert!(layout.total_lines() >= 2, "{text}");

        for (line, _) in &lines {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(state.theme.dim), "{span:?}");
                assert!(
                    span.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::ITALIC),
                    "{span:?}"
                );
                assert_eq!(span.style.bg, None, "{span:?}");
            }
        }

        assert!(
            lines
                .iter()
                .flat_map(|(line, _)| line.spans.iter())
                .any(|span| {
                    span.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::BOLD)
                }),
            "bold emphasis should survive"
        );
    }

    #[test]
    fn bands_never_leak_into_neighboring_entries() {
        let state = state(vec![
            Entry::User("hello".to_owned()),
            Entry::Info("note".to_owned()),
            tool("contents", "ok"),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);

        let visible = layout.visible_lines(&state, 0, 5);

        assert_eq!(visible[0].1, state.theme.user_bg);
        assert_eq!(visible[1].1, None, "gap lines never carry a band");
        assert_eq!(visible[2].1, None);
        assert_eq!(visible[3].1, None, "gap lines never carry a band");
        assert_eq!(visible[4].1, state.theme.tool_bg);
    }

    #[test]
    fn cursor_offsets_follow_wrapping_and_newlines() {
        assert_eq!(cursor_offset_at("", 0, 10), (0, 0));
        assert_eq!(cursor_offset_at("abc", 1, 10), (0, 1));
        assert_eq!(cursor_offset_at("a\nb", 2, 10), (1, 0));
        assert_eq!(cursor_offset("", 10), (0, 0));
        assert_eq!(cursor_offset("abc", 10), (0, 3));
        assert_eq!(cursor_offset("abcdefghij", 5), (2, 0));
        assert_eq!(cursor_offset("a\nb", 10), (1, 1));
    }

    fn patch(body: &str) -> Entry {
        Entry::Tool {
            call_id: "call-1".to_owned(),
            name: "apply_patch".to_owned(),
            status: "ok".to_owned(),
            content: "applied patch to 1 file(s): src/lib.rs (12 bytes)".to_owned(),
            truncated: false,
            title: String::new(),
            preview: ToolPreview {
                text: body.to_owned(),
                kind: crate::state::PreviewKind::Diff,
                language: None,
            },
            duration: None,
            placement: CardPlacement::default(),
        }
    }

    #[test]
    fn patch_previews_collapse_to_the_first_lines_and_expand() {
        let body = std::iter::once("@@ -1,3 +1,3 @@".to_owned())
            .chain((1..=12).map(|line| format!(" context {line}")))
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = state(vec![patch(&body)]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 80);

        let text = plain(&state, &layout).join("\n");

        assert!(text.contains("✓ Patch"), "{text}");
        assert!(text.contains("applied patch to 1 file(s)"), "{text}");
        assert!(text.contains("@@ -1,3 +1,3 @@"), "{text}");
        assert!(text.contains("context 1"), "{text}");
        assert!(text.contains("more lines, ctrl+o to expand"), "{text}");
        assert!(!text.contains("context 12"), "{text}");

        state.toggle_expanded();
        layout.sync(&state, 80);

        assert!(plain(&state, &layout).join("\n").contains("context 12"));
    }

    #[test]
    fn a_bash_card_wraps_the_whole_command_instead_of_eliding_it() {
        let command = "cargo test --workspace --all-targets --all-features -- --nocapture";
        let entry = Entry::Tool {
            call_id: "call-1".to_owned(),
            name: "bash".to_owned(),
            status: "ok".to_owned(),
            content: "[exit code 0, 1.0s]\ndone".to_owned(),
            truncated: false,
            title: command.to_owned(),
            preview: ToolPreview::default(),
            duration: None,
            placement: CardPlacement::default(),
        };
        let state = state(vec![entry]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);

        let lines = plain(&state, &layout);
        let header = lines.join("\n");
        // Wrapping can split a token across rows; the command is still whole.
        let squeezed = header.split_whitespace().collect::<String>();

        assert!(lines[0].starts_with(" ✓ Bash cargo"), "{lines:#?}");
        assert!(lines.len() > 2, "the label wraps: {lines:#?}");
        assert!(squeezed.contains("--all-features"), "{header}");
        assert!(squeezed.contains("--nocapture"), "{header}");
        assert!(
            !header.contains('…'),
            "the command is never elided: {header}"
        );
    }

    #[test]
    fn a_failed_bash_result_keeps_the_error_glyph_and_never_collapses() {
        let body = std::iter::once("[exit code 2, 0.4s]".to_owned())
            .chain((1..=12).map(|line| format!("error line {line}")))
            .collect::<Vec<_>>()
            .join("\n");
        let state = state(vec![Entry::Tool {
            call_id: "call-1".to_owned(),
            name: "bash".to_owned(),
            status: "ok".to_owned(),
            content: body,
            truncated: false,
            title: "false".to_owned(),
            preview: ToolPreview::default(),
            duration: None,
            placement: CardPlacement::default(),
        }]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 60);

        let text = plain(&state, &layout).join("\n");

        assert!(text.contains('✗'), "{text}");
        assert!(text.contains("error line 1"), "{text}");
        assert!(text.contains("error line 12"), "{text}");
        assert!(!text.contains("earlier lines"), "{text}");
        assert_eq!(layout.total_lines(), 14, "header plus every error line");
    }

    #[test]
    fn a_timed_out_success_is_rendered_like_a_failure() {
        let state = state(vec![Entry::Tool {
            call_id: "call-1".to_owned(),
            name: "bash".to_owned(),
            status: "ok".to_owned(),
            content: "[timed out after 1.0s]\npartial".to_owned(),
            truncated: false,
            title: "sleep 5".to_owned(),
            preview: ToolPreview::default(),
            duration: None,
            placement: CardPlacement::default(),
        }]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 60);

        assert!(plain(&state, &layout).join("\n").contains('✗'));
    }

    #[test]
    fn live_reasoning_grows_and_then_commits() {
        let mut state = state(Vec::new());
        let mut layout = TranscriptLayout::default();

        state.apply_event(t8ful_core::event::AgentEvent::ReasoningDelta {
            text: "thinking about".to_owned(),
        });

        layout.sync(&state, 40);

        assert_eq!(layout.total_lines(), 0, "hidden reasoning takes no space");

        state.toggle_reasoning();
        layout.sync(&state, 40);

        let live = plain(&state, &layout).join("\n");

        assert!(live.contains("thinking about"), "{live}");
        assert!(
            state.entries.is_empty(),
            "live reasoning is not committed yet"
        );

        state.apply_event(t8ful_core::event::AgentEvent::TextDelta {
            text: "answer".to_owned(),
        });
        layout.sync(&state, 40);

        let committed = plain(&state, &layout).join("\n");

        assert!(committed.contains("thinking about"), "{committed}");
        assert!(committed.contains("answer"), "{committed}");
        assert_eq!(layout.total_lines(), 3, "reasoning, gap, answer");
    }

    #[test]
    fn a_running_tool_card_shows_streamed_output_and_the_result_replaces_it() {
        let mut state = state(Vec::new());
        let mut layout = TranscriptLayout::default();

        state.apply_event(t8ful_core::event::AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "cargo test" }),
        });

        layout.sync(&state, 60);

        assert_eq!(layout.total_lines(), 1, "header only before output");
        assert!(plain(&state, &layout)[0].contains("Bash cargo test"));

        state.apply_event(t8ful_core::event::AgentEvent::ToolProgress {
            call_id: "call-1".to_owned(),
            chunk: "running test one\n".to_owned(),
            truncated: false,
        });
        layout.sync(&state, 60);

        let live = plain(&state, &layout).join("\n");

        assert!(live.contains("running test one"), "{live}");
        assert!(live.contains('…'), "the running glyph: {live}");

        state.apply_event(t8ful_core::event::AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: t8ful_core::tool::ToolStatus::Ok,
            content: "[exit code 0, 1.0s]\nrunning test one\nok".to_owned(),
            truncated: false,
            process_id: None,
        });
        layout.sync(&state, 60);

        let finished = plain(&state, &layout).join("\n");

        assert!(!finished.contains('…'), "{finished}");
        assert!(finished.contains('✓'), "{finished}");
        assert!(
            finished.lines().any(|line| line.trim() == "ok"),
            "{finished}"
        );
    }

    fn placed_card(body: &str, placement: CardPlacement) -> Entry {
        Entry::Tool {
            call_id: format!("call-{body}"),
            name: "read_file".to_owned(),
            status: "ok".to_owned(),
            content: body.to_owned(),
            truncated: false,
            title: "src/lib.rs".to_owned(),
            preview: ToolPreview::default(),
            duration: None,
            placement,
        }
    }

    #[test]
    fn a_card_renders_where_its_call_started() {
        let state = state(vec![
            Entry::Info("BEFORE".to_owned()),
            Entry::Info("AFTER".to_owned()),
            placed_card("CARD-BODY", CardPlacement { anchor: 1, seq: 0 }),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 60);

        let text = plain(&state, &layout).join("\n");
        let before = text.find("BEFORE").expect("the first entry");
        let card = text.find("CARD-BODY").expect("the card");
        let after = text.find("AFTER").expect("the later entry");

        assert!(before < card && card < after, "{text}");
    }

    #[test]
    fn a_card_anchored_at_the_tail_renders_last() {
        let state = state(vec![
            Entry::Info("FIRST".to_owned()),
            placed_card("TAIL-BODY", CardPlacement { anchor: 1, seq: 0 }),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 60);

        let text = plain(&state, &layout);
        let last = text.iter().rposition(|line| !line.is_empty()).unwrap();

        assert!(text[last].contains("TAIL-BODY"), "{text:#?}");
    }

    #[test]
    fn cards_at_one_anchor_render_in_dispatch_order() {
        // The second call finished first, so it committed first.
        let state = state(vec![
            placed_card("SECOND-BODY", CardPlacement { anchor: 0, seq: 1 }),
            placed_card("FIRST-BODY", CardPlacement { anchor: 0, seq: 0 }),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 60);

        let text = plain(&state, &layout).join("\n");
        let first = text.find("FIRST-BODY").expect("the first call");
        let second = text.find("SECOND-BODY").expect("the second call");

        assert!(first < second, "{text}");
    }

    #[test]
    fn a_live_card_stays_where_it_started_when_later_entries_arrive() {
        let mut state = state(Vec::new());
        let mut layout = TranscriptLayout::default();

        state.apply_event(t8ful_core::event::AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "make", "background": true }),
        });
        state.apply_event(t8ful_core::event::AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: t8ful_core::tool::ToolStatus::Ok,
            content: "started process-1".to_owned(),
            truncated: false,
            process_id: Some("process-1".to_owned()),
        });
        state.apply_event(t8ful_core::event::AgentEvent::TextCompleted {
            text: "LATER-TEXT".to_owned(),
        });

        layout.sync(&state, 60);

        let text = plain(&state, &layout).join("\n");
        let card = text.find("Bash make").expect("the live card");
        let later = text.find("LATER-TEXT").expect("the committed text");

        assert!(card < later, "the card moved to the tail: {text}");
    }

    #[test]
    fn a_progress_update_only_rebuilds_the_live_card() {
        let mut state = state(vec![Entry::Info("COMMITTED".to_owned())]);
        let mut layout = TranscriptLayout::default();

        state.apply_event(t8ful_core::event::AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "cargo test" }),
        });
        layout.sync(&state, 60);

        let entries = layout.entry_builds;
        let live = layout.live_builds;

        state.apply_event(t8ful_core::event::AgentEvent::ToolProgress {
            call_id: "call-1".to_owned(),
            chunk: "one\n".to_owned(),
            truncated: false,
        });
        layout.sync(&state, 60);

        assert_eq!(layout.entry_builds, entries);
        assert_eq!(layout.live_builds, live + 1);
    }

    #[test]
    fn selection_text_strips_gutters_and_whole_lines() {
        let state = state(vec![
            Entry::User("first line\nsecond line".to_owned()),
            Entry::Assistant("hello world".to_owned()),
        ]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);

        // The user block uses a three-column gutter; the assistant one column.
        assert_eq!(
            layout.selection_text(
                super::Point::new(0, 3),
                super::Point::new(1, 3 + "second line".len())
            ),
            "first line\nsecond line"
        );
        assert_eq!(
            layout.selection_text(super::Point::new(3, 1), super::Point::new(3, 6)),
            "hello"
        );
        // Ending at column zero after the last selected line drops that line.
        assert_eq!(
            layout.selection_text(super::Point::new(0, 3), super::Point::new(2, 0)),
            "first line\nsecond line"
        );
        // A selection inside one line stays inside it.
        assert_eq!(
            layout.selection_text(super::Point::new(0, 9), super::Point::new(0, 13)),
            "line"
        );
    }

    #[test]
    fn selection_text_drops_control_characters_and_keeps_columns_aligned() {
        let state = state(vec![Entry::Assistant(
            "Alpha \u{1b}]0;x\u{7}bravo \u{9b}2Jcharlie".to_owned(),
        )]);
        let mut layout = TranscriptLayout::default();

        layout.sync(&state, 40);

        // Control characters occupy no cells, so only the printable remainder
        // is copied and the column offsets count the displayed characters.
        let visible = "Alpha ]0;xbravo 2Jcharlie";
        let copied = layout.selection_text(
            super::Point::new(0, 1),
            super::Point::new(0, 1 + visible.chars().count()),
        );

        assert_eq!(copied, visible);
        assert!(!copied.chars().any(char::is_control), "{copied:?}");

        let charlie = 1 + "Alpha ]0;xbravo 2J".chars().count();

        assert_eq!(
            layout.selection_text(
                super::Point::new(0, charlie),
                super::Point::new(0, charlie + 7)
            ),
            "charlie"
        );
    }

    #[test]
    fn diff_previews_color_added_removed_and_hunk_lines() {
        let state = state(vec![patch("@@ -1 +1 @@\n-removed\n+added")]);
        let mut layout = TranscriptLayout::default();
        let theme = crate::theme::Theme::pi();

        layout.sync(&state, 80);

        let lines = layout.visible_lines(&state, 0, layout.total_lines());
        let color_for = |needle: &str| {
            lines
                .iter()
                .flat_map(|(line, _)| line.spans.iter())
                .find(|span| span.content.contains(needle))
                .and_then(|span| span.style.fg)
        };

        assert_eq!(color_for("@@ -1 +1 @@"), Some(theme.accent));
        assert_eq!(color_for("-removed"), Some(theme.error));
        assert_eq!(color_for("+added"), Some(theme.success));
    }
}
