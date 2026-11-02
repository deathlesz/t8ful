use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tokio::sync::mpsc;

use t8ful_core::provider::ReasoningEffort;

use t8ful_core::event::{AgentEvent, SequencedEvent};

use crate::bridge::{TuiBridge, TuiCommand};
use crate::clipboard::{CopyCoordinator, TextClipboard, TextCopyOutcome};
use crate::commands::COMMANDS;
use crate::input::EventReader;
use crate::notify::{Attention, Notifier, SystemNotifier};
use crate::question::PendingQuestion;
use crate::state::{AppState, Header, QuestionProgress};
use crate::terminal::TerminalGuard;
use crate::ui::{TranscriptLayout, render};

/// Minimum gap between content-driven redraws.
///
/// The loop waits on channels and one deadline instead of polling the terminal,
/// so a delta arriving mid-frame is drawn at this boundary rather than deferred
/// to a later poll. Input and turn completion bypass it.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Cap input handled between draws so a paste or key burst becomes one frame.
const MAX_INPUT_EVENTS_PER_FRAME: usize = 64;

/// Cap agent events absorbed between draws.
///
/// Input is biased first and separately capped, so a sustained delta stream
/// cannot starve the keyboard or the redraw.
const MAX_EVENTS_PER_FRAME: usize = 512;

/// Backlog at which display-only coalescing starts.
///
/// Above this mark consecutive text and reasoning deltas are joined and
/// `ToolProgress`, which is display-only, is dropped; structured events are
/// still applied in order.
const EVENT_BACKLOG_HIGH_WATER: usize = 4_096;

/// How often an in-flight clipboard copy is checked for completion.
const COPY_POLL_INTERVAL: Duration = Duration::from_millis(50);

const MOUSE_SCROLL_STEP: usize = 3;

/// Double-click window for selecting a word.
const DOUBLE_CLICK_MS: u64 = 400;

#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub header: Header,
    pub theme: crate::theme::Theme,
    /// Totals already recorded by the session this UI opens, so a resumed
    /// footer starts from the journal instead of zero.
    pub usage: crate::state::UsageTotals,
    pub show_reasoning: bool,
    pub reasoning_effort: ReasoningEffort,
    pub models: Vec<String>,
    pub max_image_encoded_bytes: usize,
    pub mouse: bool,
    /// Ring the terminal bell and raise a desktop notification.
    pub notifications: bool,
    /// Command that replaces the `notify-send` fallback.
    pub notify_command: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run the full-screen interface until the user quits.
///
/// The agent runs in a separate task; this loop only drains its events, renders,
/// and forwards input, so a slow provider or tool never blocks the UI.
pub async fn run(config: TuiConfig, bridge: TuiBridge) -> Result<(), TuiError> {
    run_with_clipboard(
        config,
        bridge,
        std::sync::Arc::new(crate::clipboard::SystemTextClipboard),
    )
    .await
}

/// Run the interface with a specific text clipboard.
///
/// Tests drive this path with a recording clipboard; the production entry point
/// keeps its signature and supplies the system implementation.
pub async fn run_with_clipboard(
    config: TuiConfig,
    bridge: TuiBridge,
    clipboard: std::sync::Arc<dyn TextClipboard>,
) -> Result<(), TuiError> {
    let notifier = std::sync::Arc::new(SystemNotifier::new(
        config.notifications,
        config.notify_command.clone(),
    ));

    run_with_services(config, bridge, clipboard, notifier).await
}

/// Run the interface with specific clipboard and notifier implementations.
///
/// Tests drive this path with recording implementations; the production entry
/// points keep their signatures and supply the system versions.
pub async fn run_with_services(
    config: TuiConfig,
    mut bridge: TuiBridge,
    clipboard: std::sync::Arc<dyn TextClipboard>,
    notifier: std::sync::Arc<dyn Notifier>,
) -> Result<(), TuiError> {
    let mut terminal = TerminalGuard::enter(config.mouse)?;
    let mut attention = Attention::new(notifier);
    let mut state = AppState::new(config.header);
    state.theme = config.theme;
    state.usage = config.usage;
    state.show_reasoning = config.show_reasoning;
    state.reasoning_effort = config.reasoning_effort;
    state.models = config.models;
    state.apply_catalog(bridge.catalog_rx.borrow_and_update().clone());
    state.apply_mcp(*bridge.mcp_rx.borrow_and_update());
    state.apply_reasoning(&bridge.reasoning_rx.borrow_and_update().clone());
    state.set_auth(*bridge.auth_rx.borrow_and_update());
    state.apply_sessions(bridge.sessions_rx.borrow_and_update().clone());
    state.apply_skills(bridge.skills_rx.borrow_and_update().clone());
    state.apply_files(bridge.files_rx.borrow_and_update().clone());
    state.apply_processes(bridge.process_rx.borrow_and_update().clone());
    state.apply_agents(bridge.agents_rx.borrow_and_update().clone());
    state.apply_tasks(bridge.tasks_rx.borrow_and_update().clone());
    state.apply_context(bridge.context_rx.borrow_and_update().limit);

    let mut copies = CopyCoordinator::new(clipboard);
    let mut layout = TranscriptLayout::default();

    // Query once, after entering the alternate screen and before terminal
    // events are read. The fallback is halfblocks, so a terminal that does not
    // answer cannot hold up startup.
    layout.detect_picker();
    let mut questions: VecDeque<PendingQuestion> = VecDeque::new();
    let mut active_question: Option<PendingQuestion> = None;
    let mut input = EventReader::spawn();
    let mut input_open = true;
    let mut dirty = true;
    // A keystroke or a finished turn is drawn at once; content arriving from
    // the provider is coalesced to one frame per `FRAME_INTERVAL`.
    let mut urgent = true;
    let mut last_draw = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);

    // The configured limit is enforced by the driver when bytes arrive; the UI
    // keeps it only so the fake clipboard boundary can be documented in state.
    let _max_image_encoded_bytes = config.max_image_encoded_bytes;

    loop {
        if !bridge.event_rx.is_empty() {
            urgent |= drain_agent_events(&mut bridge.event_rx, &mut state, &mut attention);
            dirty = true;
        }

        while let Ok(question) = bridge.question_rx.try_recv() {
            questions.push_back(question);
            urgent = true;
        }

        // A cancelled turn drops its answer channel. Never keep a stale prompt
        // in front of a newer request, and never let its answer apply later.
        questions.retain(|question| !question.is_closed());

        if active_question
            .as_ref()
            .is_some_and(PendingQuestion::is_closed)
        {
            active_question = None;
            state.question = None;
            dirty = true;
        }

        if active_question.is_none()
            && let Some(question) = questions.pop_front()
        {
            attention.question();
            state.question = Some(QuestionProgress::begin(question.request.clone()));
            active_question = Some(question);
            state.set_status("question".to_owned());
            dirty = true;
            urgent = true;
        }

        for _ in 0..MAX_INPUT_EVENTS_PER_FRAME {
            let Some(event) = input.try_recv() else {
                break;
            };

            match handle_input_event(
                &mut state,
                event,
                &bridge.command_tx,
                &layout,
                &mut active_question,
                &mut copies,
            ) {
                Handled::Ignored => {}
                Handled::Redraw => {
                    dirty = true;
                    urgent = true;
                }
                Handled::Resize => {
                    // Recompute the viewport before interpreting further keys,
                    // so a page step uses the new transcript height.
                    dirty = true;
                    urgent = true;

                    break;
                }
            }
        }

        if let Some(completion) = copies.poll() {
            finish_copy(&mut state, completion, crate::clipboard::write_osc52);
            dirty = true;
        }

        if layout.apply_image_results() {
            dirty = true;
            urgent = true;
        }

        if let Some(started) = state.turn_started {
            let phase = crate::spinner::frame_phase(started.elapsed());

            if phase != state.spinner_phase {
                state.spinner_phase = phase;
                dirty = true;
            }
        }

        // The frame deadline replaces the old 33 ms streaming-only throttle and
        // the 16 ms blocking poll: content-driven redraws are coalesced here,
        // and the loop sleeps until the next one instead of spinning.
        if dirty && (urgent || !state.busy || last_draw.elapsed() >= FRAME_INTERVAL) {
            terminal
                .terminal()
                .draw(|frame| render(frame, &mut state, &mut layout))?;
            last_draw = Instant::now();
            dirty = false;
            urgent = false;
        }

        if state.should_quit {
            let _ = bridge.command_tx.send(TuiCommand::Quit);

            break;
        }

        let mut deadline = if dirty {
            Some(last_draw + FRAME_INTERVAL)
        } else if state.busy {
            next_tick(state.turn_started)
        } else {
            None
        };

        // A copy runs in its own task; poll for its completion while it is in
        // flight instead of waiting for the next terminal event.
        if copies.busy() {
            let poll_at = Instant::now() + COPY_POLL_INTERVAL;

            deadline = Some(deadline.map_or(poll_at, |deadline| deadline.min(poll_at)));
        }

        // A preview decode runs on a worker thread; poll its channel while one
        // is in flight, even when the UI is otherwise idle.
        if layout.has_pending_previews() {
            let poll_at = Instant::now() + Duration::from_millis(40);

            deadline = Some(deadline.map_or(poll_at, |deadline| deadline.min(poll_at)));
        }

        tokio::select! {
            biased;
            event = input.recv(), if input_open => {
                match event {
                    Some(event) => {
                        match handle_input_event(
                            &mut state,
                            event,
                            &bridge.command_tx,
                            &layout,
                            &mut active_question,
                            &mut copies,
                        ) {
                            Handled::Ignored => {}
                            Handled::Redraw | Handled::Resize => {
                                dirty = true;
                                urgent = true;
                            }
                        }
                    }
                    None => input_open = false,
                }
            }
            event = bridge.event_rx.recv() => {
                if let Some(event) = event {
                    urgent |= absorb_event(&mut state, &mut attention, event.event);
                    dirty = true;
                }
            }
            // A watch branch applies the snapshot itself: awaiting `changed`
            // marks it as seen, so a later `has_changed` poll would miss it.
            () = watch_changed(&mut bridge.catalog_rx) => {
                let snapshot = bridge.catalog_rx.borrow_and_update().clone();

                state.apply_catalog(snapshot);
                dirty = true;
            }
            () = watch_changed(&mut bridge.mcp_rx) => {
                state.apply_mcp(*bridge.mcp_rx.borrow_and_update());
                dirty = true;
            }
            () = watch_changed(&mut bridge.reasoning_rx) => {
                let snapshot = bridge.reasoning_rx.borrow_and_update().clone();

                state.apply_reasoning(&snapshot);
                dirty = true;
            }
            () = watch_changed(&mut bridge.auth_rx) => {
                state.set_auth(*bridge.auth_rx.borrow_and_update());
                dirty = true;
            }
            () = watch_changed(&mut bridge.sessions_rx) => {
                let snapshot = bridge.sessions_rx.borrow_and_update().clone();

                state.apply_sessions(snapshot);
                dirty = true;
            }
            () = watch_changed(&mut bridge.skills_rx) => {
                let snapshot = bridge.skills_rx.borrow_and_update().clone();

                state.apply_skills(snapshot);
                dirty = true;
            }
            () = watch_changed(&mut bridge.files_rx) => {
                let snapshot = bridge.files_rx.borrow_and_update().clone();

                state.apply_files(snapshot);
                dirty = true;
            }
            () = watch_changed(&mut bridge.process_rx) => {
                let snapshot = bridge.process_rx.borrow_and_update().clone();

                state.apply_processes(snapshot);
                dirty = true;
            }
            () = watch_changed(&mut bridge.agents_rx) => {
                let snapshot = bridge.agents_rx.borrow_and_update().clone();

                state.apply_agents(snapshot);
                dirty = true;
            }
            () = watch_changed(&mut bridge.tasks_rx) => {
                let snapshot = bridge.tasks_rx.borrow_and_update().clone();

                state.apply_tasks(snapshot);
                dirty = true;
            }
            () = watch_changed(&mut bridge.context_rx) => {
                let snapshot = *bridge.context_rx.borrow_and_update();

                state.apply_context(snapshot.limit);
                dirty = true;
            }
            () = sleep_until(deadline) => {}
        }
    }

    input.shutdown();

    copies.shutdown().await;

    Ok(())
}

/// What the loop should do after one terminal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    /// Nothing to redraw, such as a key release.
    Ignored,
    /// Repaint, and do it now rather than at the frame deadline.
    Redraw,
    /// The terminal changed size; recompute the viewport first.
    Resize,
}

/// Apply one terminal event, shared by the burst drain and the awaiting branch.
fn handle_input_event(
    state: &mut AppState,
    event: Event,
    command_tx: &mpsc::UnboundedSender<TuiCommand>,
    layout: &TranscriptLayout,
    active_question: &mut Option<PendingQuestion>,
    copies: &mut CopyCoordinator,
) -> Handled {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(state, key, command_tx, active_question);

            Handled::Redraw
        }
        Event::Mouse(mouse) => {
            handle_mouse(state, mouse, layout, &mut |text| {
                copies.enqueue(text);
            });

            Handled::Redraw
        }
        Event::Resize(_, _) => Handled::Resize,
        _ => Handled::Ignored,
    }
}

/// A run of consecutive deltas waiting to be absorbed as one event.
enum PendingDelta {
    Text(String),
    Reasoning(String),
}

impl PendingDelta {
    fn into_event(self) -> AgentEvent {
        match self {
            Self::Text(text) => AgentEvent::TextDelta { text },
            Self::Reasoning(text) => AgentEvent::ReasoningDelta { text },
        }
    }
}

/// Absorb up to [`MAX_EVENTS_PER_FRAME`] queued events.
///
/// Returns whether any absorbed event is urgent. A backlog above
/// [`EVENT_BACKLOG_HIGH_WATER`] joins consecutive deltas and drops display-only
/// tool progress, but always applies structured events in order.
fn drain_agent_events(
    receiver: &mut mpsc::UnboundedReceiver<SequencedEvent>,
    state: &mut AppState,
    attention: &mut Attention,
) -> bool {
    let backlogged = receiver.len() >= EVENT_BACKLOG_HIGH_WATER;
    let mut urgent = false;
    let mut pending: Option<PendingDelta> = None;

    for _ in 0..MAX_EVENTS_PER_FRAME {
        let Ok(sequenced) = receiver.try_recv() else {
            break;
        };
        let event = sequenced.event;

        if backlogged && matches!(event, AgentEvent::ToolProgress { .. }) {
            continue;
        }

        if !backlogged {
            urgent |= absorb_event(state, attention, event);

            continue;
        }

        let joined = match (&mut pending, &event) {
            (Some(PendingDelta::Text(buffer)), AgentEvent::TextDelta { text }) => {
                buffer.push_str(text);
                true
            }
            (Some(PendingDelta::Reasoning(buffer)), AgentEvent::ReasoningDelta { text }) => {
                buffer.push_str(text);
                true
            }
            _ => false,
        };

        if joined {
            continue;
        }

        if let Some(buffer) = pending.take() {
            urgent |= absorb_event(state, attention, buffer.into_event());
        }

        match event {
            AgentEvent::TextDelta { text } => pending = Some(PendingDelta::Text(text)),
            AgentEvent::ReasoningDelta { text } => pending = Some(PendingDelta::Reasoning(text)),
            other => urgent |= absorb_event(state, attention, other),
        }
    }

    if let Some(buffer) = pending {
        urgent |= absorb_event(state, attention, buffer.into_event());
    }

    urgent
}

/// Apply one event and raise the notifications it implies.
///
/// The plan-choice transition is detected around `apply_event` because the
/// modal is TUI-local state, not its own agent event.
fn absorb_event(state: &mut AppState, attention: &mut Attention, event: AgentEvent) -> bool {
    let urgent = event_is_urgent(&event);
    let had_plan_choice = state.plan_choice.is_some();

    if let AgentEvent::TurnStarted { .. } = &event {
        attention.turn_started();
    }

    let finished = match &event {
        AgentEvent::TurnFinished { reason, .. } => Some(reason.clone()),
        _ => None,
    };

    state.apply_event(event);

    if !had_plan_choice && state.plan_choice.is_some() {
        attention.plan_ready();
    }

    if let Some(reason) = finished {
        attention.turn_finished(&reason);
    }

    urgent
}

/// Whether an agent event should interrupt the frame coalescing.
///
/// Token, reasoning, and tool-output deltas arrive many times per second and are
/// drawn at the next frame boundary; everything structural is drawn at once.
fn event_is_urgent(event: &AgentEvent) -> bool {
    !matches!(
        event,
        AgentEvent::TextDelta { .. }
            | AgentEvent::ReasoningDelta { .. }
            | AgentEvent::ToolProgress { .. }
            | AgentEvent::ProcessOutput { .. }
            | AgentEvent::UsageRecorded { .. }
            | AgentEvent::AgentEvent { .. }
            | AgentEvent::AgentSpawned { .. }
            | AgentEvent::AgentFinished { .. }
            | AgentEvent::TaskChanged { .. }
    )
}

/// Await one change on a watch channel, parking forever once it closes.
///
/// A closed watch would otherwise report `Err` immediately on every poll and
/// turn the `select!` into a busy loop.
async fn watch_changed<T>(rx: &mut tokio::sync::watch::Receiver<T>) {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Sleep until the frame deadline, or forever when there is nothing pending.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// The next moment the busy line changes: a spinner frame or a whole second.
fn next_tick(started: Option<Instant>) -> Option<Instant> {
    let started = started?;
    let elapsed = started.elapsed();
    let frame = crate::spinner::FRAME_INTERVAL.as_millis().max(1);
    let frames = elapsed.as_millis() / frame + 1;
    let frame_at = started + Duration::from_millis((frames * frame) as u64);
    let second_at = started + Duration::from_secs(elapsed.as_secs() + 1);

    Some(frame_at.min(second_at))
}

fn finish_copy(
    state: &mut AppState,
    completion: crate::clipboard::CopyCompletion,
    terminal_copy: impl FnOnce(&str) -> Result<crate::clipboard::CopyMethod, String>,
) {
    let lines = completion.text.lines().count().max(1);
    let mut note = String::new();
    let result = match completion.result {
        Ok(TextCopyOutcome::Copied(method)) => Ok(method),
        Ok(TextCopyOutcome::TerminalFallback { failures }) => {
            note = summarise_failures(&failures);
            terminal_copy(&completion.text).map_err(|error| {
                failures
                    .into_iter()
                    .chain([error])
                    .collect::<Vec<_>>()
                    .join("; ")
            })
        }
        Err(error) => Err(error),
    };
    state.set_status(match result {
        Ok(method) => format!("copied {lines} line(s) via {method}{note}"),
        Err(error) => format!("copy failed: {error}"),
    });
}

/// Keep a fallback reason short enough for the status line.
fn summarise_failures(failures: &[String]) -> String {
    let joined = failures.join("; ");

    if joined.is_empty() {
        return String::new();
    }

    let mut summary = joined.chars().take(120).collect::<String>();

    if summary.len() < joined.len() {
        summary.push('…');
    }

    format!(" ({summary})")
}

fn handle_mouse(
    state: &mut AppState,
    mouse: MouseEvent,
    layout: &TranscriptLayout,
    enqueue: &mut dyn FnMut(String),
) {
    if state.picker.is_some()
        || state.session_picker.is_some()
        || state.reasoning_picker.is_some()
        || state.question.is_some()
    {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            state.selection = None;
            state.scroll_up(MOUSE_SCROLL_STEP);
        }
        MouseEventKind::ScrollDown => {
            state.selection = None;
            state.scroll_down(MOUSE_SCROLL_STEP);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(point) = point_at(state, mouse.column, mouse.row) else {
                return;
            };
            let now = now_millis();
            let previous = state.selection;
            let repeat = previous.is_some_and(|selection| {
                selection.clicks >= 1
                    && now.saturating_sub(selection.click_ms) <= DOUBLE_CLICK_MS
                    && selection.anchor.line == point.line
            });
            let clicks = if repeat { 2 } else { 1 };
            let mut focus = point;

            let mut anchor = point;

            if clicks == 2
                && let Some(text) = layout.line_text(point.line)
            {
                let (start, end) = word_at(&text, point.line, point.column);

                anchor = start;
                focus = end;
            }

            state.selection = Some(crate::state::Selection {
                anchor,
                focus,
                dragging: clicks != 2,
                click_ms: now,
                clicks,
            });
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(mut selection) = state.selection else {
                return;
            };
            let area = state.transcript_area;

            if area.height == 0 {
                return;
            }

            if mouse.row < area.y {
                state.scroll_up(1);
            } else if mouse.row >= area.y + area.height {
                state.scroll_down(1);
            }

            let column = mouse
                .column
                .clamp(area.x, area.x + area.width.saturating_sub(1));
            let row = mouse
                .row
                .clamp(area.y, area.y + area.height.saturating_sub(1));
            let line = state.transcript_start + (row - area.y) as usize;

            selection.focus = crate::state::Point::new(line, (column - area.x) as usize);
            selection.dragging = true;
            state.selection = Some(selection);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(mut selection) = state.selection else {
                return;
            };

            selection.dragging = false;
            state.selection = Some(selection);

            if selection.is_empty() {
                return;
            }

            let (start, end) = selection.ordered();
            let text = layout.selection_text(start, end);

            if text.trim().is_empty() {
                return;
            }

            enqueue(text);
            state.set_status("copying…".to_owned());
        }
        _ => {}
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Transcript point under a screen cell, excluding the vertical padding.
fn point_at(state: &AppState, column: u16, row: u16) -> Option<crate::state::Point> {
    let area = state.transcript_area;

    if area.width == 0 || area.height == 0 {
        return None;
    }

    if column < area.x || column >= area.x + area.width {
        return None;
    }

    if row < area.y || row >= area.y + area.height {
        return None;
    }

    Some(crate::state::Point::new(
        state.transcript_start + (row - area.y) as usize,
        (column - area.x) as usize,
    ))
}

/// Extend a double-click to the whitespace-delimited word around `column`.
fn word_at(text: &str, line: usize, column: usize) -> (crate::state::Point, crate::state::Point) {
    let chars = text.chars().collect::<Vec<_>>();
    let column = column.min(chars.len());
    let mut start = column;
    let mut end = column;

    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }

    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }

    (
        crate::state::Point::new(line, start),
        crate::state::Point::new(line, end),
    )
}

fn handle_key(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::UnboundedSender<TuiCommand>,
    question: &mut Option<PendingQuestion>,
) {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if state.help {
        if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
            state.help = false;
            state.set_status("help closed".to_owned());
        }

        return;
    }

    if state.plan_choice.is_some() && handle_plan_choice_key(state, key, commands) {
        return;
    }

    // A key the hand-off panel does not use dismisses it and then behaves as it
    // would on the composer: typing reaches the editor, Ctrl-C cancels the turn,
    // and Ctrl-Q queues a follow-up.
    if state.plan_choice.is_some() {
        state.dismiss_plan_choice();
    }

    if state.question.is_some() {
        let mut progress = state.question.take().expect("question checked");
        let done = handle_question_key(state, &mut progress, key, question);

        if !done {
            state.question = Some(progress);
        }

        return;
    }

    if state.picker.is_some() {
        handle_picker_key(state, key, commands);

        return;
    }

    if state.session_picker.is_some() {
        handle_session_picker_key(state, key, commands);

        return;
    }

    if let Some(selected) = state.reasoning_picker {
        handle_reasoning_picker_key(state, selected, key, commands);

        return;
    }

    if state.login.is_some() {
        handle_login_key(state, key, commands);

        return;
    }

    // Alt-Up must win over the completion popup and the editor's line motion.
    if key.code == KeyCode::Up && alt {
        restore_pending(state, commands);

        return;
    }

    if state.completion_visible() {
        match key.code {
            KeyCode::Up => {
                state.move_completion(false);

                return;
            }
            KeyCode::Down => {
                state.move_completion(true);

                return;
            }
            KeyCode::Tab => {
                state.complete_selected();

                return;
            }
            KeyCode::Esc => {
                state.dismiss_completions();
                state.set_status("completion dismissed".to_owned());

                return;
            }
            KeyCode::Enter => {
                let exact = COMMANDS.iter().any(|spec| spec.name == state.input());

                if !exact && let Some(completion) = state.selected_completion() {
                    if completion.is_mention() {
                        // A partial path is accepted into the composer; an
                        // exact one falls through and submits the prompt.
                        if completion.completed_input() != state.input() {
                            state.complete_selected();

                            return;
                        }
                    } else {
                        let text = completion.completed_input();

                        state.clear_input();
                        run_command(state, commands, text.trim_end());

                        return;
                    }
                }
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char('o') if control => {
            state.toggle_expanded();
            state.set_status(if state.expanded {
                "tool output expanded".to_owned()
            } else {
                "tool output collapsed".to_owned()
            });
        }
        KeyCode::Char('r') if control => {
            state.toggle_reasoning();
            state.set_status(if state.show_reasoning {
                "reasoning shown".to_owned()
            } else {
                "reasoning hidden".to_owned()
            });
        }
        KeyCode::Char('e') if alt => {
            state.reasoning_picker = Some(state.reasoning_index());
            state.set_status("reasoning level".to_owned());
        }
        KeyCode::Char('c') if control => {
            if state.busy {
                let _ = commands.send(TuiCommand::Cancel);
                // "cancelling" is a request; `turn finished (cancelled)` from the
                // driver is what confirms it actually stopped.
                state.set_status("cancelling…".to_owned());
            } else {
                state.should_quit = true;
            }
        }
        KeyCode::Char('d') if control => {
            if state.input().is_empty() {
                state.should_quit = true;
            } else {
                state.edit_input(crate::edit::Edit::Delete);
            }
        }
        KeyCode::Char('v') if control => {
            let _ = commands.send(TuiCommand::PasteClipboard);
        }
        KeyCode::Char('q') if control && !state.input().trim().is_empty() => {
            let text = state.input.take();
            state.dismiss_plan_choice();
            state.refresh_completions();
            let _ = commands.send(TuiCommand::Queue(text));
        }
        KeyCode::Tab => {
            let next = match state.header.mode {
                t8ful_core::policy::Mode::Coding => t8ful_core::policy::Mode::Plan,
                t8ful_core::policy::Mode::Plan => t8ful_core::policy::Mode::Coding,
            };

            let _ = commands.send(TuiCommand::SetMode(next));
            state.set_status(format!("{} mode requested", next.label()));
        }
        KeyCode::Char('?') if !control && !alt => {
            if state.input().is_empty() {
                state.help = true;
                state.set_status("help".to_owned());
            } else {
                state.edit_input(crate::edit::Edit::Insert('?'));
            }
        }
        KeyCode::Esc => {
            if state.selection.is_some() {
                state.selection = None;
                state.set_status("selection cleared".to_owned());
            } else if state.busy {
                let _ = commands.send(TuiCommand::Cancel);
                state.set_status("cancelling…".to_owned());
            } else if !state.input().is_empty() {
                state.clear_input();
            }
        }
        KeyCode::Enter => {
            enter(state, commands);
        }
        KeyCode::PageUp => state.scroll_up(state.page_step()),
        KeyCode::PageDown => state.scroll_down(state.page_step()),
        _ => {
            if let Some(edit) = crate::edit::binding(&key) {
                state.edit_input(edit);
            }
        }
    }
}

/// Fold pending steering and queued follow-ups back into the composer, oldest
/// first, and tell the driver to forget its copies.
fn restore_pending(state: &mut AppState, commands: &mpsc::UnboundedSender<TuiCommand>) {
    let Some(count) = state.restore_pending() else {
        state.set_status("no pending messages".to_owned());

        return;
    };

    let _ = commands.send(TuiCommand::Dequeue);
    state.set_status(format!(
        "restored {count} pending message{} to the composer",
        if count == 1 { "" } else { "s" }
    ));
}

fn handle_login_key(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::UnboundedSender<TuiCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            state.close_login();
            state.set_status("login cancelled".to_owned());
        }
        KeyCode::Enter => {
            let Some(modal) = state.login.as_ref() else {
                return;
            };

            if modal.buffer.as_str().trim().is_empty() {
                state.set_status("enter an API key, or press Esc to cancel".to_owned());

                return;
            }

            let provider = modal.provider.clone();

            let Some(key) = state.close_login() else {
                return;
            };

            let _ = commands.send(TuiCommand::Login {
                provider,
                key: crate::bridge::Secret::new(key),
            });
            state.set_status("saving the API key…".to_owned());
        }
        _ => {
            if let Some(edit) = crate::edit::binding(&key)
                && edit != crate::edit::Edit::Newline
            {
                state.edit_login(edit);
            }
        }
    }
}

/// Handle one key while the plan hand-off panel is up.
///
/// Returns whether the panel used the key. Anything else dismisses the panel and
/// keeps its ordinary meaning.
fn handle_plan_choice_key(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::UnboundedSender<TuiCommand>,
) -> bool {
    match key.code {
        KeyCode::Up => {
            state.move_plan_choice(false);

            true
        }
        KeyCode::Down => {
            state.move_plan_choice(true);

            true
        }
        KeyCode::Enter => {
            // Read the action without consuming the choice: `apply_plan_action`
            // needs the plan text to seed the next session.
            let action = state
                .plan_choice
                .as_ref()
                .map(crate::state::PlanChoice::action);

            if let Some(action) = action {
                apply_plan_action(state, commands, action);
            }

            true
        }
        KeyCode::Char('1') => {
            apply_plan_action(state, commands, crate::state::PlanAction::ImplementHere);

            true
        }
        KeyCode::Char('2') => {
            apply_plan_action(
                state,
                commands,
                crate::state::PlanAction::ImplementInNewSession,
            );

            true
        }
        KeyCode::Char('3') | KeyCode::Esc => {
            state.dismiss_plan_choice();
            state.set_status("staying in plan mode".to_owned());

            true
        }
        _ => false,
    }
}

fn apply_plan_action(
    state: &mut AppState,
    commands: &mpsc::UnboundedSender<TuiCommand>,
    action: crate::state::PlanAction,
) {
    let plan = state
        .plan_choice
        .as_ref()
        .map(|choice| choice.plan.clone())
        .unwrap_or_default();

    state.dismiss_plan_choice();

    match action {
        crate::state::PlanAction::ImplementHere => {
            let _ = commands.send(TuiCommand::SetMode(t8ful_core::policy::Mode::Coding));
            let _ = commands.send(TuiCommand::Submit(
                "Implement the plan you presented.".to_owned(),
            ));
            state.set_status("implementing the plan…".to_owned());
        }
        crate::state::PlanAction::ImplementInNewSession => {
            let _ = commands.send(TuiCommand::NewSession {
                mode: Some(t8ful_core::policy::Mode::Coding),
                seed: Some(format!("Approved plan:\n\n{plan}\n\nImplement it now.")),
            });
            state.set_status("starting a new session…".to_owned());
        }
        crate::state::PlanAction::Stay => {
            state.set_status("staying in plan mode".to_owned());
        }
    }
}

fn handle_question_key(
    state: &mut AppState,
    progress: &mut QuestionProgress,
    key: KeyEvent,
    question: &mut Option<PendingQuestion>,
) -> bool {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Esc => {
            if progress.is_custom() {
                progress.custom_mode = false;

                false
            } else if progress.back() {
                state.set_status("back to the previous question".to_owned());

                false
            } else {
                if let Some(pending) = question.take() {
                    drop(pending);
                }

                state.set_status("question cancelled".to_owned());

                true
            }
        }
        KeyCode::Left | KeyCode::BackTab => {
            if progress.back() {
                state.set_status("back to the previous question".to_owned());
            }

            false
        }
        KeyCode::Right | KeyCode::Tab => {
            if progress.forward() {
                state.set_status("next question".to_owned());
            }

            false
        }
        KeyCode::Char('t') if !progress.is_custom() && !progress.review() => {
            progress.toggle_custom();
            progress.highlight = progress
                .current()
                .map(|question| question.options.len())
                .unwrap_or(0);

            false
        }
        KeyCode::Enter if progress.review() => match progress.submit_review() {
            Some(answers) => deliver_answers(state, question, answers),
            None => {
                state.set_status("answer every question first".to_owned());

                false
            }
        },
        KeyCode::Enter if progress.is_custom() => match progress.submit_custom() {
            Some(answers) => deliver_answers(state, question, answers),
            None => false,
        },
        KeyCode::Up if !progress.is_custom() => {
            progress.move_highlight(false);

            false
        }
        KeyCode::Down if !progress.is_custom() => {
            progress.move_highlight(true);

            false
        }
        KeyCode::Enter if !progress.is_custom() => {
            if progress.on_custom_row() {
                progress.toggle_custom();

                return false;
            }

            if progress.on_done_row() {
                return finish_multi_select(state, progress, question);
            }

            choose_question(state, progress, progress.highlight, question)
        }
        KeyCode::Char(' ') if !progress.is_custom() && !progress.review() => {
            if progress.on_custom_row() {
                progress.toggle_custom();

                return false;
            }

            if progress.on_done_row() {
                return finish_multi_select(state, progress, question);
            }

            if progress.is_multi() {
                progress.toggle_option(progress.highlight);
            }

            false
        }
        KeyCode::Char('1') if !progress.is_custom() && !progress.review() && !shift => {
            choose_question(state, progress, 0, question)
        }
        KeyCode::Char('2') if !progress.is_custom() && !progress.review() && !shift => {
            choose_question(state, progress, 1, question)
        }
        KeyCode::Char('3') if !progress.is_custom() && !progress.review() && !shift => {
            choose_question(state, progress, 2, question)
        }
        KeyCode::Char('4') if !progress.is_custom() && !progress.review() && !shift => {
            choose_question(state, progress, 3, question)
        }
        _ => {
            if progress.is_custom()
                && let Some(edit) = crate::edit::binding(&key)
                && edit != crate::edit::Edit::Newline
            {
                progress.custom.apply(edit);
            }

            false
        }
    }
}

fn choose_question(
    state: &mut AppState,
    progress: &mut QuestionProgress,
    option: usize,
    question: &mut Option<PendingQuestion>,
) -> bool {
    if progress.is_multi() {
        progress.toggle_option(option);

        return false;
    }

    match progress.choose(option) {
        Some(answers) => deliver_answers(state, question, answers),
        None => {
            if progress.review() {
                state.set_status("review the answers".to_owned());
            }

            false
        }
    }
}

fn finish_multi_select(
    state: &mut AppState,
    progress: &mut QuestionProgress,
    question: &mut Option<PendingQuestion>,
) -> bool {
    if progress.checked.is_empty() && progress.custom_draft.is_none() {
        state.set_status("choose at least one option".to_owned());

        return false;
    }

    match progress.done() {
        Some(answers) => deliver_answers(state, question, answers),
        None => {
            if progress.review() {
                state.set_status("review the answers".to_owned());
            }

            false
        }
    }
}

fn deliver_answers(
    state: &mut AppState,
    question: &mut Option<PendingQuestion>,
    answers: Vec<t8ful_core::question::QuestionAnswer>,
) -> bool {
    if let Some(pending) = question.take() {
        let _ = pending.answer(answers);
    }

    state.set_status("question answered".to_owned());

    true
}

fn handle_picker_key(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::UnboundedSender<TuiCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            state.close_picker();
            state.set_status("model picker closed".to_owned());
        }
        KeyCode::Up => state.move_picker(false),
        KeyCode::Down => state.move_picker(true),
        KeyCode::Enter => {
            if let Some(model) = state.selected_model() {
                let _ = commands.send(TuiCommand::SelectModel(model));
                state.close_picker();
                state.set_status("model change queued for the next turn boundary".to_owned());
            }
        }
        _ => {
            if let Some(edit) = crate::edit::binding(&key)
                && edit != crate::edit::Edit::Newline
            {
                state.edit_picker_filter(edit);
            }
        }
    }
}

fn handle_session_picker_key(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::UnboundedSender<TuiCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            state.close_session_picker();
            state.set_status("session picker closed".to_owned());
        }
        KeyCode::Up => state.move_session_picker(false),
        KeyCode::Down => state.move_session_picker(true),
        KeyCode::Enter => {
            let Some(session) = state.selected_session() else {
                return;
            };

            let _ = commands.send(TuiCommand::ResumeSession(session.id));
            state.close_session_picker();
            state.set_status("switching session…".to_owned());
        }
        _ => {
            if let Some(edit) = crate::edit::binding(&key)
                && edit != crate::edit::Edit::Newline
            {
                state.edit_session_filter(edit);
            }
        }
    }
}

fn enter(state: &mut AppState, commands: &mpsc::UnboundedSender<TuiCommand>) {
    let text = state.input().trim().to_owned();

    if text.is_empty() {
        state.clear_input();

        return;
    }

    if text.starts_with('/') {
        state.clear_input();
        run_command(state, commands, &text);

        return;
    }

    // A session can start without a model (so `/model` and `/login` are
    // reachable), but a prompt must never be sent before one is selected.
    if state.header.model.trim().is_empty() {
        state.set_status("no model selected — use /model or /model <id>".to_owned());

        return;
    }

    if state.busy {
        // The steer is only a transcript line once the agent delivers it, so
        // the composer text moves to the pending list instead of the log.
        state.clear_input();
        state.dismiss_plan_choice();
        state.remember_steering(text.clone());
        let _ = commands.send(TuiCommand::Steer(text));
        state.set_status("steering after the current step".to_owned());

        return;
    }

    if let Some(prompt) = state.submit() {
        let _ = commands.send(TuiCommand::Submit(prompt));
    }
}

fn run_command(state: &mut AppState, commands: &mpsc::UnboundedSender<TuiCommand>, text: &str) {
    let (command, argument) = text
        .split_once(char::is_whitespace)
        .map(|(command, argument)| (command, argument.trim()))
        .unwrap_or((text, ""));

    match command {
        "/plan" => {
            let _ = commands.send(TuiCommand::SetMode(t8ful_core::policy::Mode::Plan));
            state.set_status("plan mode requested".to_owned());
        }
        "/code" | "/coding" => {
            let _ = commands.send(TuiCommand::SetMode(t8ful_core::policy::Mode::Coding));
            state.set_status("coding mode requested".to_owned());
        }
        "/agents" => {
            if state.agents.is_empty() {
                state.push_info("no child agents have been started in this session");
            } else {
                let wanted = argument.trim();

                for agent in state.agents.clone() {
                    if !wanted.is_empty() && agent.id != wanted {
                        continue;
                    }

                    let mut line = format!(
                        "{} {} {} {}s {} step{}{}",
                        agent.id,
                        agent.status,
                        agent.role,
                        agent.elapsed_ms / 1000,
                        agent.steps,
                        if agent.steps == 1 { "" } else { "s" },
                        agent
                            .task
                            .as_ref()
                            .map(|task| format!(" task {task}"))
                            .unwrap_or_default(),
                    );

                    if let Some(path) = &agent.worktree {
                        line.push_str(&format!(
                            "\n  worktree {path} on {}",
                            agent.branch.as_deref().unwrap_or("(detached)")
                        ));
                        line.push_str(&format!("\n  inspect with `git -C {path} diff`"));
                    }

                    if let Some(report) = &agent.report {
                        line.push_str(&format!("\n{report}"));
                    }

                    state.push_info(line);
                }
            }
        }
        "/tasks" => {
            if state.tasks.is_empty() {
                state.push_info("no tasks have been recorded in this session");
            } else {
                let lines = state
                    .tasks
                    .iter()
                    .map(|task| {
                        let owner = task
                            .owner
                            .as_ref()
                            .map(|owner| format!(" ({owner})"))
                            .unwrap_or_default();
                        let blocked = if task.blocked { " [blocked]" } else { "" };
                        let deps = if task.deps.is_empty() {
                            String::new()
                        } else {
                            format!(" after {}", task.deps.join(", "))
                        };

                        format!(
                            "{} {} {}{}{}{}",
                            task.id, task.state, task.title, owner, deps, blocked
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                state.push_info(lines);
            }
        }
        "/login" => {
            if argument.is_empty() || argument.eq_ignore_ascii_case("openrouter") {
                state.open_login("openrouter");
            } else {
                state.set_status(format!(
                    "unknown provider {argument}; use /login openrouter"
                ));
            }
        }
        "/logout" => {
            let _ = commands.send(TuiCommand::Logout);
            state.set_status("logging out…".to_owned());
        }
        "/model" => {
            if argument.is_empty() {
                state.close_picker();

                if state.models.is_empty() && !state.catalog_pending() {
                    state.set_status("no discovered models; use /model <id>".to_owned());
                } else {
                    state.picker = Some(0);

                    if state.models.is_empty() {
                        state.set_status("loading models…".to_owned());
                    }
                }
            } else {
                let _ = commands.send(TuiCommand::SelectModel(argument.to_owned()));
                state.set_status("model change queued for the next turn boundary".to_owned());
            }
        }
        "/attach" => {
            if argument.is_empty() {
                state.set_status("usage: /attach <path>".to_owned());
            } else {
                let _ = commands.send(TuiCommand::Attach {
                    path: Some(argument.to_owned()),
                    bytes: None,
                    media_type: None,
                });
                state.set_status("validating attachment…".to_owned());
            }
        }
        "/new" => {
            let _ = commands.send(TuiCommand::NewSession {
                mode: None,
                seed: None,
            });
        }
        "/sessions" => {
            // Print the snapshot immediately, then refresh it for the next call.
            if state.sessions_loaded() {
                state
                    .entries
                    .push(crate::state::Entry::Info(state.session_report()));
                state.set_status(format!("{} saved session(s)", state.sessions.len()));
            } else {
                state.set_status("loading sessions…".to_owned());
            }

            let _ = commands.send(TuiCommand::ListSessions);
        }
        "/resume" => {
            if argument.is_empty() {
                let _ = commands.send(TuiCommand::ListSessions);
                state.open_session_picker();
                state.set_status("resume session".to_owned());
            } else {
                let _ = commands.send(TuiCommand::ResumeSession(argument.to_owned()));
            }
        }
        "/skill" => {
            if state.busy {
                state.push_info("finish the current turn before loading a skill");
            } else if argument.is_empty() {
                if state.skills_loaded() {
                    state
                        .entries
                        .push(crate::state::Entry::Info(state.skill_report()));
                    state.set_status(format!("{} skill(s)", state.skills.len()));
                } else {
                    state.set_status("loading skills…".to_owned());
                }
            } else {
                let (name, prompt) = argument
                    .split_once(char::is_whitespace)
                    .map(|(name, prompt)| (name, prompt.trim()))
                    .unwrap_or((argument, ""));

                if state.skills.iter().any(|skill| skill.name == name) {
                    let prompt = (!prompt.is_empty()).then(|| prompt.to_owned());
                    let _ = commands.send(TuiCommand::ActivateSkill {
                        name: name.to_owned(),
                        prompt: prompt.clone(),
                    });

                    if prompt.is_some() {
                        state.request_turn("requesting the model…");
                    }

                    state.set_status(format!("loading skill {name}…"));
                } else {
                    state.push_info(format!(
                        "unknown skill {name}; use /skill to list the installed skills"
                    ));
                }
            }
        }
        "/reasoning" => {
            if argument.is_empty() {
                state.reasoning_picker = Some(state.reasoning_index());
                state.set_status("reasoning level".to_owned());
            } else if let Some(effort) = ReasoningEffort::parse(argument) {
                set_reasoning_effort(state, commands, effort);
            } else {
                state.set_status(format!(
                    "invalid reasoning level; this model offers {}",
                    state
                        .reasoning_options()
                        .iter()
                        .map(|level| level.label())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        "/show-reasoning" => {
            state.toggle_reasoning();
            state.set_status(if state.show_reasoning {
                "reasoning shown".to_owned()
            } else {
                "reasoning hidden".to_owned()
            });
        }
        "/process" => {
            let (action, id) = argument
                .split_once(char::is_whitespace)
                .map(|(action, id)| (action, id.trim()))
                .unwrap_or((argument, ""));

            match action {
                "" | "list" => {
                    if state.processes_loaded() {
                        state
                            .entries
                            .push(crate::state::Entry::Info(state.process_report()));
                        state.set_status(format!(
                            "{} background process(es)",
                            state.process_views.len()
                        ));
                    } else {
                        state.set_status("loading processes…".to_owned());
                    }

                    let _ = commands.send(TuiCommand::ProcessList);
                }
                "view" => {
                    if id.is_empty() {
                        state.set_status("usage: /process view <id>".to_owned());
                    } else {
                        let _ = commands.send(TuiCommand::ProcessView(id.to_owned()));
                        state.set_status(format!("reading {id}…"));
                    }
                }
                "stop" => {
                    if id.is_empty() {
                        state.set_status("usage: /process stop <id>".to_owned());
                    } else {
                        state.expect_process_stop(id);
                        let _ = commands.send(TuiCommand::ProcessStop(id.to_owned()));
                        state.set_status(format!("stopping {id}…"));
                    }
                }
                other => {
                    state.set_status(format!(
                        "unknown /process action {other}; use list, view, or stop"
                    ));
                }
            }
        }
        "/quit" => state.should_quit = true,
        "/help" => {
            state.help = true;
            state.set_status("help".to_owned());
        }
        other => {
            state.set_status(format!("unknown command {other}; try /help"));
        }
    }
}

fn handle_reasoning_picker_key(
    state: &mut AppState,
    selected: usize,
    key: KeyEvent,
    commands: &mpsc::UnboundedSender<TuiCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            state.reasoning_picker = None;
            state.set_status("reasoning picker closed".to_owned());
        }
        KeyCode::Up => state.move_reasoning_picker(false),
        KeyCode::Down => state.move_reasoning_picker(true),
        KeyCode::Enter => {
            let Some(effort) = state.reasoning_at(selected) else {
                return;
            };

            state.reasoning_picker = None;
            set_reasoning_effort(state, commands, effort);
        }
        _ => {}
    }
}

fn set_reasoning_effort(
    state: &mut AppState,
    commands: &mpsc::UnboundedSender<TuiCommand>,
    effort: ReasoningEffort,
) {
    if !state.reasoning_options().contains(&effort) {
        state.set_status(format!(
            "this model does not offer `{}`; choose one of {}",
            effort.label(),
            state
                .reasoning_options()
                .iter()
                .map(|level| level.label())
                .collect::<Vec<_>>()
                .join(", ")
        ));

        return;
    }

    let _ = commands.send(TuiCommand::SetReasoningEffort(effort));
    state.reasoning_effort = effort;
    state.set_status(format!("reasoning effort: {}", effort.label()));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use t8ful_core::event::{AgentEvent, SequencedEvent};
    use t8ful_core::policy::Mode;
    use t8ful_core::provider::ReasoningEffort;
    use t8ful_core::question::{Question, QuestionOption, QuestionRequest};
    use tokio::sync::{mpsc, oneshot};

    use super::{
        AppState, EVENT_BACKLOG_HIGH_WATER, Header, MAX_EVENTS_PER_FRAME, QuestionProgress,
        TuiCommand, absorb_event, drain_agent_events, handle_key, handle_mouse, run_command,
    };
    use crate::bridge::{SessionSnapshot, SessionSummary, SkillView, SkillsSnapshot};
    use crate::notify::{
        Attention, Notifier, PLAN_READY, QUESTION_WAITING, RecordingNotifier, TURN_FAILED,
        TURN_FINISHED,
    };
    use crate::question::PendingQuestion;
    use crate::state::Entry;

    fn state() -> AppState {
        AppState::new(Header {
            model: "vendor/model".to_owned(),
            mode: Mode::Coding,
            session_id: "session-1".to_owned(),
            project_root: PathBuf::from("/work/project"),
            launch_dir: PathBuf::from("/work/project"),
            version: "0.1.0".to_owned(),
            branch: None,
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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

    fn load_sessions(state: &mut AppState, sessions: Vec<SessionSummary>) {
        state.apply_sessions(SessionSnapshot::Loaded { sessions });
    }

    fn skill(name: &str, description: &str, commit: Option<&str>) -> SkillView {
        SkillView {
            name: name.to_owned(),
            description: description.to_owned(),
            source: "user".to_owned(),
            commit: commit.map(str::to_owned),
        }
    }

    fn load_skills(state: &mut AppState, skills: Vec<SkillView>) {
        state.apply_skills(SkillsSnapshot {
            loaded: true,
            skills,
        });
    }

    fn type_text(state: &mut AppState, commands: &mpsc::UnboundedSender<TuiCommand>, text: &str) {
        for character in text.chars() {
            press(state, commands, KeyCode::Char(character));
        }
    }

    fn press(state: &mut AppState, commands: &mpsc::UnboundedSender<TuiCommand>, code: KeyCode) {
        press_with(state, commands, code, KeyModifiers::NONE);
    }

    fn press_with(
        state: &mut AppState,
        commands: &mpsc::UnboundedSender<TuiCommand>,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) {
        let mut question = None;

        handle_key(
            state,
            KeyEvent::new(code, modifiers),
            commands,
            &mut question,
        );
    }

    fn question_request() -> QuestionRequest {
        QuestionRequest {
            questions: vec![Question {
                header: "Scope".to_owned(),
                question: "Which file?".to_owned(),
                options: vec![
                    QuestionOption {
                        label: "a.rs".to_owned(),
                        description: "first".to_owned(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "b.rs".to_owned(),
                        description: "second".to_owned(),
                        preview: None,
                    },
                ],
                multi_select: false,
            }],
        }
    }

    #[test]
    fn mode_and_model_commands_wait_for_the_driver_before_updating_the_header() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        run_command(&mut state, &commands, "/plan");
        assert_eq!(state.header.mode, Mode::Coding);
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SetMode(Mode::Plan))
        ));

        run_command(&mut state, &commands, "/model vendor/other");
        assert_eq!(state.header.model, "vendor/model");
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SelectModel(model)) if model == "vendor/other"
        ));
    }

    #[test]
    fn the_help_command_opens_the_overlay() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        run_command(&mut state, &commands, "/help");

        assert!(state.help);
    }

    #[test]
    fn tab_requests_the_opposite_mode() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        press(&mut state, &commands, KeyCode::Tab);

        assert_eq!(state.header.mode, Mode::Coding);
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SetMode(Mode::Plan))
        ));

        state.apply_event(AgentEvent::ModeChanged {
            mode: Mode::Plan,
            previous: None,
        });

        press(&mut state, &commands, KeyCode::Tab);

        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SetMode(Mode::Coding))
        ));
    }

    #[test]
    fn ctrl_o_and_ctrl_r_toggle_display_only_layout() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();
        let mut question = None;

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &commands,
            &mut question,
        );
        assert!(state.expanded);

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            &commands,
            &mut question,
        );
        assert!(state.show_reasoning);
    }

    #[test]
    fn show_reasoning_and_the_shortcuts_open_the_picker() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        run_command(&mut state, &commands, "/show-reasoning");

        assert!(state.show_reasoning);
        assert_eq!(state.status, "reasoning shown");

        state.reasoning_effort = ReasoningEffort::Medium;
        run_command(&mut state, &commands, "/reasoning");

        assert!(state.reasoning_picker.is_some());
        assert_eq!(
            state.reasoning_picker,
            Some(state.reasoning_index()),
            "the current effort is highlighted"
        );
        assert!(state.show_reasoning, "/reasoning no longer toggles display");

        state.close_picker();
        state.reasoning_picker = None;
        state.set_input("hello world");
        state.input.set_cursor(0);

        press_with(
            &mut state,
            &commands,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.input_editor().cursor(), 0);

        press_with(
            &mut state,
            &commands,
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.input_editor().cursor(), 11, "ctrl-e ends the line");
        assert_eq!(state.reasoning_picker, None);

        press_with(&mut state, &commands, KeyCode::Char('e'), KeyModifiers::ALT);

        assert_eq!(state.reasoning_picker, Some(state.reasoning_index()));
    }

    #[test]
    fn reasoning_levels_and_navigation_stay_inside_the_models_own_levels() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        run_command(&mut state, &commands, "/reasoning high");

        assert_eq!(state.reasoning_effort, ReasoningEffort::High);
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SetReasoningEffort(ReasoningEffort::High))
        ));

        run_command(&mut state, &commands, "/reasoning louder");

        assert!(
            state.status.contains("invalid reasoning level"),
            "{}",
            state.status
        );

        state.reasoning_levels = vec![ReasoningEffort::Default, ReasoningEffort::Off];
        state.reasoning_picker = Some(0);
        state.move_reasoning_picker(true);
        assert_eq!(state.reasoning_picker, Some(1));
        state.move_reasoning_picker(true);
        assert_eq!(state.reasoning_picker, Some(1), "the last row is the end");

        state.reasoning_levels = vec![
            ReasoningEffort::Default,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ];
        state.reasoning_picker = Some(0);
        state.move_reasoning_picker(true);
        assert_eq!(state.reasoning_picker, Some(1));
        state.move_reasoning_picker(false);
        assert_eq!(state.reasoning_picker, Some(0));
    }

    #[test]
    fn an_unsupported_level_is_refused_but_extra_levels_are_accepted() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        // A model without reasoning offers only default and off.
        state.reasoning_levels = vec![ReasoningEffort::Default, ReasoningEffort::Off];
        run_command(&mut state, &commands, "/reasoning high");

        assert_eq!(state.reasoning_effort, ReasoningEffort::Default);
        assert!(received.try_recv().is_err());
        assert!(
            state.status.contains("does not offer `high`"),
            "{}",
            state.status
        );

        state.reasoning_levels = vec![
            ReasoningEffort::Default,
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ];
        run_command(&mut state, &commands, "/reasoning max");

        assert_eq!(state.reasoning_effort, ReasoningEffort::Max);
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SetReasoningEffort(ReasoningEffort::Max))
        ));
    }

    #[test]
    fn a_model_switch_clamps_an_effort_it_no_longer_offers() {
        let mut state = state();

        state.reasoning_effort = ReasoningEffort::Max;
        state.apply_reasoning(&crate::bridge::ReasoningSnapshot::Loaded {
            levels: vec![
                ReasoningEffort::Default,
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::High,
            ],
        });

        assert_eq!(state.reasoning_effort, ReasoningEffort::High);
        assert_eq!(state.reasoning_options(), &state.reasoning_levels);
    }

    #[test]
    fn emacs_bindings_edit_every_text_field() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        state.set_input("alpha beta");
        state.input.set_cursor(5);

        // Ctrl-K kills to the end of the line, Ctrl-Y yanks it back.
        press_with(
            &mut state,
            &commands,
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.input(), "alpha");
        press_with(
            &mut state,
            &commands,
            KeyCode::Char('y'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.input(), "alpha beta");

        // Ctrl-W kills the word before the cursor.
        press_with(
            &mut state,
            &commands,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.input(), "alpha ");

        // Alt-B moves by a word and never inserts `b`.
        press_with(&mut state, &commands, KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(state.input(), "alpha ");
        assert_eq!(state.input_editor().cursor(), 0);

        press_with(&mut state, &commands, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(state.input_editor().cursor(), 6);

        press_with(
            &mut state,
            &commands,
            KeyCode::Char('h'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.input(), "alpha");

        press_with(
            &mut state,
            &commands,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.input(), "alpha");
        assert!(!state.should_quit, "ctrl-d deletes before it quits");

        state.clear_input();
        press_with(
            &mut state,
            &commands,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        );
        assert!(state.should_quit, "ctrl-d on an empty composer quits");

        // The same bindings edit the model and session picker filters.
        state.should_quit = false;
        state.models = vec!["vendor/large".to_owned()];
        run_command(&mut state, &commands, "/model");
        type_text(&mut state, &commands, "lrg");

        press_with(
            &mut state,
            &commands,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        press_with(
            &mut state,
            &commands,
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(
            state.picker_filter(),
            "",
            "ctrl-k kills from the line start"
        );

        press_with(
            &mut state,
            &commands,
            KeyCode::Char('y'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.picker_filter(), "lrg", "ctrl-y restores the kill");

        state.close_picker();

        state.open_session_picker();
        press_with(
            &mut state,
            &commands,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(state.session_picker_filter(), "");
    }

    #[test]
    fn slash_completions_persist_until_tab_completes_or_esc_dismisses() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        press(&mut state, &commands, KeyCode::Char('/'));

        assert!(state.completion_visible());
        assert!(!state.completions.is_empty());

        press(&mut state, &commands, KeyCode::Char('p'));
        assert!(state.completion_visible());
        assert_eq!(
            state
                .selected_completion()
                .map(|completion| completion.spec().expect("command").name),
            Some("/plan")
        );

        state.set_input("/rea".to_owned());
        state.refresh_completions();
        press(&mut state, &commands, KeyCode::Tab);

        assert_eq!(state.input(), "/reasoning ");
        assert!(
            state.completion_visible(),
            "levels are offered after the space"
        );
        assert_eq!(state.completions.len(), state.reasoning_levels.len());

        press(&mut state, &commands, KeyCode::Esc);

        assert!(!state.completion_visible());

        state.set_input("/pl".to_owned());
        state.refresh_completions();
        press(&mut state, &commands, KeyCode::Esc);

        assert!(!state.completion_visible());
        assert_eq!(state.input(), "/pl");

        press(&mut state, &commands, KeyCode::Char('a'));

        assert!(state.completion_visible());
        assert_eq!(
            state
                .selected_completion()
                .map(|completion| completion.spec().expect("command").name),
            Some("/plan")
        );
    }

    #[test]
    fn the_plan_hand_off_starts_a_seeded_new_session() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();
        let mut question = None;

        state.header.mode = Mode::Plan;
        state.plan_choice = Some(crate::state::PlanChoice {
            plan: "# Ship it".to_owned(),
            highlight: 0,
        });

        handle_key(
            &mut state,
            key(KeyCode::Char('2')),
            &commands,
            &mut question,
        );

        match received.try_recv() {
            Ok(TuiCommand::NewSession {
                mode: Some(Mode::Coding),
                seed: Some(seed),
            }) => {
                assert!(seed.contains("# Ship it"), "{seed}");
                assert!(seed.contains("Implement it now."), "{seed}");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        assert!(state.plan_choice.is_none());

        // Enter on the same highlighted row takes the same path.
        state.plan_choice = Some(crate::state::PlanChoice {
            plan: "# Ship it".to_owned(),
            highlight: 1,
        });

        handle_key(&mut state, key(KeyCode::Enter), &commands, &mut question);

        match received.try_recv() {
            Ok(TuiCommand::NewSession {
                mode: Some(Mode::Coding),
                seed: Some(seed),
            }) => {
                assert!(seed.contains("# Ship it"), "{seed}");
                assert!(seed.contains("Implement it now."), "{seed}");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        assert!(state.plan_choice.is_none());
    }

    #[test]
    fn implementing_here_or_escape_decides_the_plan() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();
        let mut question = None;

        state.plan_choice = Some(crate::state::PlanChoice {
            plan: "# Ship it".to_owned(),
            highlight: 0,
        });

        handle_key(
            &mut state,
            key(KeyCode::Char('1')),
            &commands,
            &mut question,
        );

        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SetMode(Mode::Coding))
        ));
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::Submit(prompt)) if prompt == "Implement the plan you presented."
        ));

        state.plan_choice = Some(crate::state::PlanChoice {
            plan: "# Ship it".to_owned(),
            highlight: 0,
        });
        handle_key(&mut state, key(KeyCode::Esc), &commands, &mut question);

        assert!(state.plan_choice.is_none());
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn a_question_can_move_back_to_a_previous_question() {
        let request = t8ful_core::question::QuestionRequest {
            questions: vec![
                t8ful_core::question::Question {
                    header: "Scope".to_owned(),
                    question: "How wide?".to_owned(),
                    options: vec![t8ful_core::question::QuestionOption {
                        label: "Narrow".to_owned(),
                        description: String::new(),
                        preview: None,
                    }],
                    multi_select: false,
                },
                t8ful_core::question::Question {
                    header: "Tests".to_owned(),
                    question: "Which suite?".to_owned(),
                    options: vec![t8ful_core::question::QuestionOption {
                        label: "Fast".to_owned(),
                        description: String::new(),
                        preview: None,
                    }],
                    multi_select: false,
                },
            ],
        };
        let (sender, _receiver) = oneshot::channel();
        let mut pending = Some(PendingQuestion {
            request: request.clone(),
            respond: sender,
        });
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        state.question = Some(QuestionProgress::begin(request));

        handle_key(&mut state, key(KeyCode::Char('1')), &commands, &mut pending);
        assert_eq!(state.question.as_ref().expect("question").index, 1);

        handle_key(&mut state, key(KeyCode::Left), &commands, &mut pending);
        assert_eq!(state.question.as_ref().expect("question").index, 0);
    }

    /// One step of a scripted session, applied the way the UI loop applies it.
    enum Step {
        Started,
        Delta,
        Finished(&'static str),
        Error,
        PlanHandOff,
        Question,
    }

    #[test]
    fn attention_raises_one_brief_message_per_turn() {
        let cases: &[(&str, &[Step], &[&str])] = &[
            (
                "a finished turn notifies once",
                &[Step::Started, Step::Delta, Step::Finished("stop")],
                &[TURN_FINISHED],
            ),
            (
                "a failed turn says so briefly",
                &[Step::Started, Step::Finished("error")],
                &[TURN_FAILED],
            ),
            (
                "a plan hand-off replaces the turn end",
                &[Step::Started, Step::PlanHandOff, Step::Finished("stop")],
                &[PLAN_READY],
            ),
            (
                "a mid-turn error stays in the transcript",
                &[Step::Started, Step::Error, Step::Finished("stop")],
                &[TURN_FINISHED],
            ),
            (
                "a question always notifies",
                &[Step::Started, Step::Question],
                &[QUESTION_WAITING],
            ),
            (
                "a second turn may notify again",
                &[
                    Step::Started,
                    Step::Finished("stop"),
                    Step::Started,
                    Step::Finished("stop"),
                ],
                &[TURN_FINISHED, TURN_FINISHED],
            ),
        ];

        for (name, steps, expected) in cases {
            let mut state = state();
            let recorder = std::sync::Arc::new(RecordingNotifier::default());
            let mut attention =
                Attention::new(std::sync::Arc::clone(&recorder) as std::sync::Arc<dyn Notifier>);

            for step in *steps {
                match step {
                    Step::Started => {
                        absorb_event(
                            &mut state,
                            &mut attention,
                            AgentEvent::TurnStarted { turn: 1 },
                        );
                    }
                    Step::Delta => {
                        absorb_event(
                            &mut state,
                            &mut attention,
                            AgentEvent::TextDelta {
                                text: "streaming".to_owned(),
                            },
                        );
                    }
                    Step::Finished(reason) => {
                        absorb_event(
                            &mut state,
                            &mut attention,
                            AgentEvent::TurnFinished {
                                turn: 1,
                                reason: (*reason).to_owned(),
                            },
                        );
                    }
                    Step::Error => {
                        absorb_event(
                            &mut state,
                            &mut attention,
                            AgentEvent::Error {
                                message: "boom".to_owned(),
                            },
                        );
                    }
                    Step::PlanHandOff => {
                        state.header.mode = Mode::Plan;

                        for event in [
                            AgentEvent::ToolCallStarted {
                                call_id: "call-1".to_owned(),
                                tool: "present_plan".to_owned(),
                                arguments: serde_json::json!({ "plan": "# Plan" }),
                            },
                            AgentEvent::ToolResult {
                                call_id: "call-1".to_owned(),
                                tool: "present_plan".to_owned(),
                                status: t8ful_core::tool::ToolStatus::Ok,
                                content: String::new(),
                                truncated: false,
                                process_id: None,
                            },
                        ] {
                            absorb_event(&mut state, &mut attention, event);
                        }
                    }
                    Step::Question => attention.question(),
                }
            }

            let messages = recorder.messages();

            assert_eq!(
                messages.iter().map(String::as_str).collect::<Vec<_>>(),
                *expected,
                "{name}"
            );
        }
    }

    #[test]
    fn enter_accepts_a_partial_command_and_submits_an_exact_one() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        for character in "/pla".chars() {
            press(&mut state, &commands, KeyCode::Char(character));
        }

        press(&mut state, &commands, KeyCode::Enter);

        assert!(state.input().is_empty());
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SetMode(Mode::Plan))
        ));

        for character in "/help".chars() {
            press(&mut state, &commands, KeyCode::Char(character));
        }

        assert!(state.completion_visible());

        press(&mut state, &commands, KeyCode::Enter);

        assert!(state.help);
    }

    #[test]
    fn enter_without_a_model_keeps_the_composer_and_sends_nothing() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        state.header.model.clear();
        state.set_input("hello".to_owned());
        state.refresh_completions();

        press(&mut state, &commands, KeyCode::Enter);

        assert_eq!(state.input(), "hello");
        assert!(received.try_recv().is_err());
        assert_eq!(
            state.status,
            "no model selected — use /model or /model <id>"
        );
    }

    #[test]
    fn commands_still_run_without_a_model() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        state.header.model.clear();
        state.set_input("/help".to_owned());
        state.refresh_completions();

        press(&mut state, &commands, KeyCode::Enter);

        assert!(state.help);
    }

    #[test]
    fn enter_with_arguments_submits_literally() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        state.set_input("/model vendor/other".to_owned());
        state.refresh_completions();

        press(&mut state, &commands, KeyCode::Enter);

        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SelectModel(model)) if model == "vendor/other"
        ));
    }

    #[test]
    fn down_moves_the_completion_selection() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        press(&mut state, &commands, KeyCode::Char('/'));

        let first = state
            .selected_completion()
            .map(|completion| completion.spec().expect("command").name);

        press(&mut state, &commands, KeyCode::Down);

        assert_ne!(
            state
                .selected_completion()
                .map(|completion| completion.spec().expect("command").name),
            first
        );
    }

    #[tokio::test]
    async fn question_option_keys_and_custom_text_choose_an_answer() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();
        let request = question_request();
        let (respond, answer) = oneshot::channel();
        let mut pending = Some(PendingQuestion {
            request: request.clone(),
            respond,
        });

        state.question = Some(QuestionProgress::begin(request));

        handle_key(&mut state, key(KeyCode::Down), &commands, &mut pending);

        assert_eq!(state.question.as_ref().expect("question").highlight, 1);

        handle_key(&mut state, key(KeyCode::Enter), &commands, &mut pending);

        let answers = answer.await.expect("answer");
        assert_eq!(answers[0].selected, vec![1]);
        assert_eq!(answers[0].text(&question_request().questions[0]), "b.rs");
        assert!(pending.is_none());

        let (commands, _received) = mpsc::unbounded_channel();
        let request = question_request();
        let (respond, answer) = oneshot::channel();
        let mut pending = Some(PendingQuestion {
            request: request.clone(),
            respond,
        });

        state.question = Some(QuestionProgress::begin(request));

        handle_key(&mut state, key(KeyCode::Char('2')), &commands, &mut pending);

        let answers = answer.await.expect("answer");
        let question = question_request();
        assert_eq!(answers[0].selected, vec![1]);
        assert_eq!(answers[0].text(&question.questions[0]), "b.rs");
        assert!(pending.is_none());

        let (commands, _received) = mpsc::unbounded_channel();
        let request = question_request();
        let (respond, answer) = oneshot::channel();
        let mut pending = Some(PendingQuestion {
            request: request.clone(),
            respond,
        });

        state.question = Some(QuestionProgress::begin(request));

        handle_key(&mut state, key(KeyCode::Char('t')), &commands, &mut pending);

        for character in "something else".chars() {
            handle_key(
                &mut state,
                key(KeyCode::Char(character)),
                &commands,
                &mut pending,
            );
        }

        handle_key(&mut state, key(KeyCode::Enter), &commands, &mut pending);

        let answers = answer.await.expect("answer");
        let question = question_request();
        assert!(answers[0].selected.is_empty());
        assert_eq!(answers[0].custom.as_deref(), Some("something else"));
        assert_eq!(answers[0].text(&question.questions[0]), "something else");
        assert!(answers[0].is_custom());
    }

    #[test]
    fn alt_up_restores_pending_text_and_outranks_the_completion_popup() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();
        let mut question = None;

        state.set_input("draft".to_owned());

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
            &commands,
            &mut question,
        );

        assert_eq!(state.status, "no pending messages");
        assert_eq!(state.input(), "draft");
        assert!(received.try_recv().is_err());

        state.remember_steering("steer".to_owned());
        state.queue = vec!["first".to_owned(), "second".to_owned()];
        state.queue_paused = true;
        state.set_input("draft".to_owned());

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
            &commands,
            &mut question,
        );

        assert_eq!(state.input(), "steer\n\nfirst\n\nsecond\n\ndraft");
        assert!(state.pending_steering().is_empty());
        assert!(state.queue.is_empty());
        assert!(!state.queue_paused);
        assert_eq!(state.status, "restored 3 pending messages to the composer");
        assert!(matches!(received.try_recv(), Ok(TuiCommand::Dequeue)));

        // Alt-Up is handled before the completion popup's own Up binding.
        state.set_input("/re".to_owned());
        state.refresh_completions();
        state.queue = vec!["queued".to_owned()];

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
            &commands,
            &mut question,
        );

        assert_eq!(state.input(), "queued\n\n/re");
        assert!(matches!(received.try_recv(), Ok(TuiCommand::Dequeue)));
    }

    #[test]
    fn login_masks_the_key_and_refuses_an_empty_submission() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        run_command(&mut state, &commands, "/login openrouter");
        press(&mut state, &commands, KeyCode::Enter);

        assert!(state.login.is_some());
        assert!(
            state.status.contains("enter an API key"),
            "{}",
            state.status
        );
        assert!(received.try_recv().is_err());

        run_command(&mut state, &commands, "/login");

        assert!(state.login.is_some());
        assert_eq!(state.login.as_ref().unwrap().provider, "openrouter");

        for character in "sk-or-key".chars() {
            press(&mut state, &commands, KeyCode::Char(character));
        }

        assert_eq!(state.login.as_ref().unwrap().masked(), "•••••••••");
        assert!(!format!("{:?}", state.login).contains("sk-or-key"));

        press(&mut state, &commands, KeyCode::Enter);

        assert!(state.login.is_none());
        assert!(
            state
                .login
                .as_ref()
                .is_none_or(|login| login.buffer.is_empty())
        );

        match received.try_recv() {
            Ok(TuiCommand::Login { provider, key }) => {
                assert_eq!(provider, "openrouter");
                assert_eq!(key.expose(), "sk-or-key");
                assert!(!format!("{key:?}").contains("sk-or-key"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        assert!(received.try_recv().is_err());
    }

    #[test]
    fn esc_clears_the_login_buffer_and_closes_the_modal() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        run_command(&mut state, &commands, "/login");

        for character in "secret".chars() {
            press(&mut state, &commands, KeyCode::Char(character));
        }

        press(&mut state, &commands, KeyCode::Esc);

        assert!(state.login.is_none());
        assert_eq!(state.status, "login cancelled");
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn an_unknown_login_provider_is_rejected_and_logout_is_forwarded() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        run_command(&mut state, &commands, "/login elsewhere");

        assert!(state.login.is_none());
        assert!(
            state.status.contains("unknown provider"),
            "{}",
            state.status
        );

        run_command(&mut state, &commands, "/logout");

        assert!(matches!(received.try_recv(), Ok(TuiCommand::Logout)));
    }

    #[test]
    fn queue_and_paste_are_no_longer_commands() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        for removed in ["/queue hello", "/paste"] {
            run_command(&mut state, &commands, removed);

            assert!(state.status.contains("unknown command"), "{}", state.status);
            assert!(received.try_recv().is_err(), "{removed}");
        }
    }

    #[test]
    fn bare_resume_opens_the_picker_and_enter_resumes_the_highlighted_session() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        load_sessions(
            &mut state,
            vec![
                session("session-new", "fix the picker", 30),
                session("session-old", "write the docs", 10),
            ],
        );
        type_text(&mut state, &commands, "/resume");
        press(&mut state, &commands, KeyCode::Enter);

        assert_eq!(state.session_picker, Some(0));
        assert_eq!(state.status, "resume session");
        assert!(matches!(received.try_recv(), Ok(TuiCommand::ListSessions)));
        assert!(
            received.try_recv().is_err(),
            "a bare /resume must not resume the first row"
        );

        state.session_picker_push_char('o');
        state.session_picker_push_char('l');
        state.session_picker_push_char('d');

        press(&mut state, &commands, KeyCode::Enter);

        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::ResumeSession(id)) if id == "session-old"
        ));
        assert!(state.session_picker.is_none());
        assert_eq!(state.status, "switching session…");
    }

    #[test]
    fn session_picker_typing_and_navigation_stay_inside_the_filtered_rows() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        load_sessions(
            &mut state,
            vec![
                session("session-alpha", "fix the picker", 30),
                session("session-beta", "write the docs", 10),
                session("session-1", "current session", 50),
            ],
        );
        state.open_session_picker();

        assert_eq!(
            state.session_picker_rows().len(),
            2,
            "current session hidden"
        );

        press(&mut state, &commands, KeyCode::Up);
        assert_eq!(state.session_picker, Some(0));

        press(&mut state, &commands, KeyCode::Down);
        press(&mut state, &commands, KeyCode::Down);
        press(&mut state, &commands, KeyCode::Down);
        assert_eq!(state.session_picker, Some(1));

        // A title fragment narrows the list to one row and the selection back
        // to the top.
        type_text(&mut state, &commands, "doc");

        let rows = state.session_picker_rows();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "session-beta");
        assert_eq!(state.session_picker_filter(), "doc");
        assert_eq!(state.session_picker, Some(0));
    }

    #[test]
    fn esc_closes_the_session_picker_and_tab_completes_a_resume_argument() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        state.open_session_picker();
        press(&mut state, &commands, KeyCode::Esc);

        assert!(state.session_picker.is_none());
        assert_eq!(state.status, "session picker closed");
        assert!(received.try_recv().is_err());

        load_sessions(
            &mut state,
            vec![session("session-alpha", "fix the picker", 30)],
        );
        type_text(&mut state, &commands, "/resume alp");

        assert!(state.completion_visible());

        press(&mut state, &commands, KeyCode::Tab);

        assert_eq!(state.input(), "/resume session-alpha");
    }

    #[test]
    fn sessions_prints_the_snapshot_and_requests_a_refresh() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        load_sessions(
            &mut state,
            vec![
                session("session-1", "current session", now),
                session("session-old", "write the docs", now - 90),
            ],
        );
        run_command(&mut state, &commands, "/sessions");

        assert_eq!(state.status, "2 saved session(s)");
        assert!(matches!(received.try_recv(), Ok(TuiCommand::ListSessions)));

        let Entry::Info(report) = state.entries.last().expect("report entry") else {
            panic!("not an info entry");
        };

        assert!(report.lines().next().unwrap().starts_with("→ session-1"));
        assert!(report.contains("write the docs"), "{report}");
        assert!(report.contains("project"), "{report}");
        assert!(report.contains(" just now"), "{report}");
    }

    #[test]
    fn sessions_report_shows_the_newest_message_age() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        load_sessions(
            &mut state,
            vec![
                // Three hours since the newest message, but only a minute since
                // the session started: the report shows the message age.
                session("session-quiet", "left it running", now - 3 * 3_600),
                session("session-chatty", "just wrote", now - 60),
            ],
        );
        run_command(&mut state, &commands, "/sessions");

        let Entry::Info(report) = state.entries.last().expect("report entry") else {
            panic!("not an info entry");
        };
        let rows = report.lines().collect::<Vec<_>>();

        assert!(rows[0].contains("session-chatty"), "{report}");
        assert!(rows[0].contains(" 1m "), "the age column: {}", rows[0]);
        assert!(rows[1].contains("session-quiet"), "{report}");
        assert!(rows[1].contains(" 3h "), "the age column: {}", rows[1]);
    }

    #[test]
    fn sessions_waits_for_the_first_scan() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        run_command(&mut state, &commands, "/sessions");

        assert_eq!(state.status, "loading sessions…");
        assert!(state.entries.is_empty());
        assert!(matches!(received.try_recv(), Ok(TuiCommand::ListSessions)));
    }

    #[test]
    fn skill_lists_the_catalog_with_source_and_commit() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        load_skills(
            &mut state,
            vec![skill(
                "review",
                "Review a change.",
                Some("0123456789abcdef"),
            )],
        );
        run_command(&mut state, &commands, "/skill");

        assert_eq!(state.status, "1 skill(s)");

        let Entry::Info(report) = state.entries.last().expect("report entry") else {
            panic!("not an info entry");
        };

        assert!(
            report.contains("review — Review a change. — user — 01234567"),
            "{report}"
        );
    }

    #[test]
    fn at_completion_inserts_a_project_path_without_losing_the_prompt() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        state.files = vec!["src/main.rs".to_owned()];

        for character in "explain @src/ma".chars() {
            press(&mut state, &commands, KeyCode::Char(character));
        }

        assert!(state.completion_visible());
        assert_eq!(
            state.selected_completion().map(|row| row.label()),
            Some("src/main.rs".to_owned())
        );

        press(&mut state, &commands, KeyCode::Tab);

        assert_eq!(state.input(), "explain @src/main.rs");
        assert!(state.entries.is_empty());
    }

    #[test]
    fn enter_accepts_a_partial_mention_and_submits_an_exact_one() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        state.files = vec!["src/main.rs".to_owned()];

        for character in "read @src/ma".chars() {
            press(&mut state, &commands, KeyCode::Char(character));
        }

        press(&mut state, &commands, KeyCode::Enter);

        assert_eq!(state.input(), "read @src/main.rs");
        assert!(state.entries.is_empty());
        assert!(received.try_recv().is_err());

        press(&mut state, &commands, KeyCode::Enter);

        assert_eq!(state.input(), "");
        assert!(
            state.entries.is_empty(),
            "the driver echoes the prompt after journaling it"
        );
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::Submit(prompt)) if prompt == "read @src/main.rs"
        ));

        state.apply_event(AgentEvent::UserMessage {
            text: "read @src/main.rs".to_owned(),
            source: t8ful_core::event::UserMessageSource::Submitted,
            images: Vec::new(),
        });

        assert_eq!(
            state.entries,
            vec![Entry::User("read @src/main.rs".to_owned())]
        );
    }

    #[test]
    fn skill_activation_defers_the_card_to_the_driver_event() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        load_skills(&mut state, vec![skill("review", "Review a change.", None)]);
        run_command(&mut state, &commands, "/skill review");

        assert_eq!(state.status, "loading skill review…");
        assert!(state.entries.is_empty());
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::ActivateSkill { name, prompt: None }) if name == "review"
        ));
    }

    #[test]
    fn skill_activation_can_carry_a_prompt_in_the_same_command() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        load_skills(&mut state, vec![skill("review", "Review a change.", None)]);
        run_command(&mut state, &commands, "/skill review check the diff");

        assert_eq!(state.status, "loading skill review…");
        assert!(state.busy);
        assert!(state.entries.is_empty());
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::ActivateSkill { name, prompt: Some(prompt) })
                if name == "review" && prompt == "check the diff"
        ));

        // The skill card arrives first; the prompt follows it, matching the
        // order a resumed journal rebuilds.
        state.apply_event(AgentEvent::SkillLoaded {
            name: "review".to_owned(),
            content: "SKILL-BODY".to_owned(),
            truncated: false,
        });

        assert!(matches!(state.entries[0], Entry::Tool { .. }));
        assert_eq!(
            state.entries.len(),
            1,
            "the prompt is echoed only once the driver journals it"
        );

        state.apply_event(AgentEvent::UserMessage {
            text: "check the diff".to_owned(),
            source: t8ful_core::event::UserMessageSource::Submitted,
            images: Vec::new(),
        });

        assert_eq!(state.entries[1], Entry::User("check the diff".to_owned()));
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.status, "requesting the model…");
    }

    #[test]
    fn a_failed_skill_activation_drops_the_attached_prompt() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        load_skills(&mut state, vec![skill("review", "Review a change.", None)]);
        run_command(&mut state, &commands, "/skill review check the diff");
        state.apply_event(AgentEvent::Error {
            message: "skill failed to load".to_owned(),
        });

        assert!(!state.busy);
        assert!(
            state
                .entries
                .iter()
                .all(|entry| !matches!(entry, Entry::User(_)))
        );
        assert_eq!(state.status, "error");
    }

    #[test]
    fn skill_rejects_an_unknown_name_without_sending_a_command() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        load_skills(&mut state, vec![skill("review", "Review a change.", None)]);
        run_command(&mut state, &commands, "/skill missing");

        assert!(
            state.entries.iter().any(
                |entry| matches!(entry, Entry::Info(text) if text.contains("unknown skill missing"))
            ),
            "{:?}",
            state.entries
        );
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn skill_is_refused_during_a_turn() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        load_skills(&mut state, vec![skill("review", "Review a change.", None)]);
        state.busy = true;
        run_command(&mut state, &commands, "/skill review");

        assert!(
            state.entries.iter().any(|entry| matches!(
                entry,
                Entry::Info(text) if text == "finish the current turn before loading a skill"
            )),
            "{:?}",
            state.entries
        );
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn tab_completes_a_skill_argument_from_the_catalog() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        load_skills(
            &mut state,
            vec![
                skill("review", "Review a change.", None),
                skill("release", "Prepare a release.", None),
            ],
        );
        type_text(&mut state, &commands, "/skill rev");

        assert!(state.completion_visible());

        press(&mut state, &commands, KeyCode::Tab);

        assert_eq!(state.input(), "/skill review");
    }

    #[test]
    fn typing_narrows_the_model_picker_and_enter_applies_the_highlighted_model() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        state.models = vec![
            "vendor/large".to_owned(),
            "vendor/small".to_owned(),
            "other/model".to_owned(),
        ];
        run_command(&mut state, &commands, "/model");
        assert_eq!(state.picker, Some(0));

        type_text(&mut state, &commands, "lrg");

        assert_eq!(state.picker_rows(), vec![0]);

        press(&mut state, &commands, KeyCode::Enter);

        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::SelectModel(model)) if model == "vendor/large"
        ));
        assert!(state.picker.is_none());
        assert_eq!(state.picker_filter(), "");

        // Navigation never leaves the filtered rows.
        state.models = vec![
            "vendor/large".to_owned(),
            "vendor/large-mini".to_owned(),
            "vendor/small".to_owned(),
        ];
        run_command(&mut state, &commands, "/model");
        type_text(&mut state, &commands, "large");

        assert_eq!(state.picker_rows(), vec![0, 1]);

        press(&mut state, &commands, KeyCode::Down);
        press(&mut state, &commands, KeyCode::Down);
        press(&mut state, &commands, KeyCode::Down);

        assert_eq!(state.picker, Some(1));
    }

    #[test]
    fn esc_closes_the_model_picker_and_a_catalog_update_keeps_the_highlight() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        state.models = vec!["vendor/large".to_owned()];
        run_command(&mut state, &commands, "/model");
        type_text(&mut state, &commands, "lar");

        assert_eq!(state.picker_filter(), "lar");

        press(&mut state, &commands, KeyCode::Esc);

        assert!(state.picker.is_none());
        assert_eq!(state.picker_filter(), "");
        assert_eq!(state.status, "model picker closed");

        // A refreshed catalog keeps the selected row when its filter matches.
        state.models = vec!["vendor/large".to_owned(), "vendor/small".to_owned()];
        run_command(&mut state, &commands, "/model");
        type_text(&mut state, &commands, "small");
        assert_eq!(state.selected_model().as_deref(), Some("vendor/small"));

        state.apply_catalog(crate::bridge::CatalogSnapshot::Loaded {
            models: vec![
                "vendor/small".to_owned(),
                "vendor/large".to_owned(),
                "vendor/tiny".to_owned(),
            ],
        });

        assert_eq!(state.picker_filter(), "small");
        assert_eq!(state.selected_model().as_deref(), Some("vendor/small"));
    }

    #[derive(Default)]
    struct RecordingClipboard {
        copied: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingClipboard {
        fn enqueue(&self, text: String) {
            self.copied.lock().unwrap().push(text);
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn transcript_state(entries: Vec<Entry>) -> (AppState, crate::ui::TranscriptLayout) {
        let mut state = state();

        state.entries = entries;

        let mut layout = crate::ui::TranscriptLayout::default();

        layout.sync(&state, 40);
        state.transcript_area = Rect::new(0, 2, 40, 6);
        state.max_scroll = 20;

        (state, layout)
    }

    #[test]
    fn dragging_over_the_transcript_copies_the_selected_text() {
        let (mut state, layout) = transcript_state(vec![Entry::User("first line".to_owned())]);
        let clipboard = RecordingClipboard::default();

        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 13, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 13, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );

        assert_eq!(
            clipboard
                .copied
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_slice(),
            ["first line"]
        );
        assert_eq!(state.status, "copying…");
        assert!(state.selection.is_some_and(|selection| !selection.dragging));
    }

    #[test]
    fn a_click_without_a_drag_copies_nothing() {
        let (mut state, layout) = transcript_state(vec![Entry::User("first line".to_owned())]);
        let clipboard = RecordingClipboard::default();

        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 3, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );

        assert!(clipboard.copied.lock().unwrap().is_empty());
    }

    #[test]
    fn a_double_click_selects_the_word_under_the_cursor() {
        let (mut state, layout) = transcript_state(vec![Entry::User("alpha beta".to_owned())]);
        let clipboard = RecordingClipboard::default();

        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 6, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 6, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );

        let selection = state.selection.expect("selection");

        assert_eq!(selection.clicks, 2);
        assert_eq!((selection.anchor.column, selection.focus.column), (3, 8));

        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 6, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );

        assert_eq!(
            clipboard
                .copied
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_slice(),
            ["alpha"]
        );
    }

    #[test]
    fn dragging_past_the_transcript_edges_scrolls_one_line() {
        let (mut state, layout) = transcript_state(vec![Entry::User("first line".to_owned())]);

        state.scroll = 5;

        let clipboard = RecordingClipboard::default();

        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 3),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );

        state.scroll = 5;
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 3, 1),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );

        assert_eq!(state.scroll, 6, "dragging above the viewport scrolls up");

        state.scroll = 5;
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 3, 90),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );

        assert_eq!(state.scroll, 4, "dragging below the viewport scrolls down");
    }

    #[test]
    fn the_event_drain_is_budgeted_and_leaves_the_rest_queued() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut attention = Attention::new(std::sync::Arc::new(RecordingNotifier::default()));
        let mut state = state();

        for index in 0..(MAX_EVENTS_PER_FRAME + 10) {
            tx.send(SequencedEvent {
                seq: index as u64,
                event: AgentEvent::TextDelta {
                    text: "x".to_owned(),
                },
            })
            .unwrap();
        }

        let urgent = drain_agent_events(&mut rx, &mut state, &mut attention);

        assert!(!urgent);
        assert_eq!(rx.len(), 10, "the drain must stop at its budget");
        assert_eq!(state.streaming, "x".repeat(MAX_EVENTS_PER_FRAME));
    }

    #[test]
    fn a_backlogged_drain_coalesces_deltas_and_drops_only_progress() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut attention = Attention::new(std::sync::Arc::new(RecordingNotifier::default()));
        let mut state = state();

        tx.send(SequencedEvent {
            seq: 0,
            event: AgentEvent::ToolCallStarted {
                call_id: "call-1".to_owned(),
                tool: "bash".to_owned(),
                arguments: serde_json::json!({}),
            },
        })
        .unwrap();
        tx.send(SequencedEvent {
            seq: 1,
            event: AgentEvent::ToolProgress {
                call_id: "call-1".to_owned(),
                chunk: "display only".to_owned(),
                truncated: false,
            },
        })
        .unwrap();
        tx.send(SequencedEvent {
            seq: 2,
            event: AgentEvent::Warning {
                message: "keep me".to_owned(),
            },
        })
        .unwrap();

        for index in 0..509 {
            tx.send(SequencedEvent {
                seq: 3 + index,
                event: AgentEvent::TextDelta {
                    text: "x".to_owned(),
                },
            })
            .unwrap();
        }

        for index in 0..EVENT_BACKLOG_HIGH_WATER {
            tx.send(SequencedEvent {
                seq: 1_000 + index as u64,
                event: AgentEvent::TextDelta {
                    text: "y".to_owned(),
                },
            })
            .unwrap();
        }

        drain_agent_events(&mut rx, &mut state, &mut attention);

        assert_eq!(state.streaming, "x".repeat(509));
        assert_eq!(
            state.tool_runs[0].progress, "",
            "display-only progress must be dropped under backlog"
        );
        assert!(
            state
                .entries
                .iter()
                .any(|entry| matches!(entry, Entry::Info(text) if text.contains("keep me"))),
            "{:?}",
            state.entries
        );
        assert_eq!(rx.len(), EVENT_BACKLOG_HIGH_WATER);
    }

    #[test]
    fn esc_clears_a_selection_before_it_cancels() {
        let (mut state, layout) = transcript_state(vec![Entry::User("first line".to_owned())]);
        let clipboard = RecordingClipboard::default();

        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 2),
            &layout,
            &mut |text| clipboard.enqueue(text),
        );

        assert!(state.selection.is_some());
        state.busy = true;

        let (commands, mut received) = mpsc::unbounded_channel();

        press(&mut state, &commands, KeyCode::Esc);

        assert!(state.selection.is_none());
        assert!(received.try_recv().is_err(), "a selection is not a cancel");
    }

    #[test]
    fn process_command_completions_offer_the_three_actions() {
        let mut state = state();
        let (commands, _received) = mpsc::unbounded_channel();

        type_text(&mut state, &commands, "/process ");

        let labels = state
            .completions
            .iter()
            .map(crate::commands::Completion::label)
            .collect::<Vec<_>>();

        assert_eq!(
            labels,
            vec!["/process list", "/process view", "/process stop"]
        );
    }

    #[test]
    fn process_reports_use_the_published_snapshot() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();

        let empty = state.clone();

        state.apply_processes(crate::bridge::ProcessSnapshot::Loaded {
            processes: Vec::new(),
        });
        run_command(&mut state, &commands, "/process list");

        assert_eq!(
            state.entries.last(),
            Some(&Entry::Info(
                "no background processes have been started".to_owned()
            ))
        );
        assert!(matches!(received.try_recv(), Ok(TuiCommand::ProcessList)));

        let mut state = empty;
        state.apply_processes(crate::bridge::ProcessSnapshot::Loaded {
            processes: vec![crate::bridge::ProcessView {
                id: "process-1".to_owned(),
                command: "cargo test".to_owned(),
                state: "running".to_owned(),
                exit_code: None,
                signal: None,
                elapsed_ms: 1_500,
                total_bytes: 12,
                truncated: false,
                output_incomplete: false,
                cleanup_error: None,
            }],
        });
        run_command(&mut state, &commands, "/process");
        assert!(matches!(received.try_recv(), Ok(TuiCommand::ProcessList)));

        let Entry::Info(report) = state.entries.last().expect("report") else {
            panic!("not an info entry");
        };

        assert!(report.contains("process-1"), "{report}");
        assert!(report.contains("running"), "{report}");
        assert!(report.contains("cargo test"), "{report}");

        run_command(&mut state, &commands, "/process view process-1");
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::ProcessView(id)) if id == "process-1"
        ));

        run_command(&mut state, &commands, "/process stop process-1");
        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::ProcessStop(id)) if id == "process-1"
        ));

        run_command(&mut state, &commands, "/process kill process-1");
        assert!(
            state.status.contains("unknown /process action kill"),
            "{}",
            state.status
        );

        run_command(&mut state, &commands, "/process view");
        assert_eq!(state.status, "usage: /process view <id>");

        run_command(&mut state, &commands, "/process dance");
        assert!(
            state.status.contains("unknown /process action dance"),
            "{}",
            state.status
        );
    }

    #[test]
    fn ctrl_q_still_queues_a_follow_up() {
        let mut state = state();
        let (commands, mut received) = mpsc::unbounded_channel();
        let mut question = None;

        state.set_input("next".to_owned());

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &commands,
            &mut question,
        );

        assert!(matches!(
            received.try_recv(),
            Ok(TuiCommand::Queue(text)) if text == "next"
        ));
    }
}

/// The chosen option index from [`ask`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskOutcome {
    /// The option at this index was chosen.
    Chosen(usize),
    /// The user dismissed the prompt.
    Cancelled,
    /// The terminal could not be used, so no answer was inferred.
    Unavailable,
}

/// Ask one question before the session starts and return the chosen option.
///
/// Startup decisions (trusting a project) must be made before settings are
/// resolved, which is before the main loop exists. Rather than duplicate the
/// question grammar, this drives the same [`QuestionProgress`] state and the
/// same renderer for exactly one question, then returns the row the user
/// picked. `Esc` cancels; it never picks an option on the user's behalf.
pub async fn ask(
    endpoints: &crate::bridge::TuiEndpoints,
    request: t8ful_core::question::QuestionRequest,
) -> AskOutcome {
    let count = request
        .questions
        .first()
        .map(|question| question.options.len())
        .unwrap_or(0);

    if count == 0 {
        return AskOutcome::Unavailable;
    }

    let mut terminal = match TerminalGuard::enter(false) {
        Ok(terminal) => terminal,
        Err(_) => return AskOutcome::Unavailable,
    };
    let mut state = AppState::new(Header {
        model: String::new(),
        mode: t8ful_core::policy::Mode::Coding,
        session_id: String::new(),
        project_root: std::path::PathBuf::new(),
        launch_dir: std::path::PathBuf::new(),
        version: String::new(),
        branch: None,
    });
    let mut layout = TranscriptLayout::default();
    let mut progress = QuestionProgress::begin(request);
    // The question is answered locally: no `PendingQuestion` channel is
    // involved, because nothing is waiting on a router for this decision.
    let mut pending: Option<PendingQuestion> = None;
    let mut input = EventReader::spawn();
    let _ = &endpoints;

    state.question = Some(QuestionProgress::begin(progress.request.clone()));
    let outcome = loop {
        if terminal
            .terminal()
            .draw(|frame| render(frame, &mut state, &mut layout))
            .is_err()
        {
            break AskOutcome::Unavailable;
        }

        let Some(event) = input.try_recv() else {
            tokio::time::sleep(Duration::from_millis(16)).await;

            continue;
        };

        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let answered = handle_question_key(&mut state, &mut progress, key, &mut pending);

                if answered {
                    break match progress.answers.first() {
                        Some(answer) => match answer.selected.first() {
                            Some(index) => AskOutcome::Chosen(*index),
                            None => AskOutcome::Cancelled,
                        },
                        None => AskOutcome::Cancelled,
                    };
                }

                // Mirror the answered progress into the renderer's copy so the
                // highlight follows the keys.
                state.question = Some(QuestionProgress {
                    request: progress.request.clone(),
                    index: progress.index,
                    answers: progress.answers.clone(),
                    custom_mode: progress.custom_mode,
                    custom: progress.custom.clone(),
                    custom_draft: progress.custom_draft.clone(),
                    checked: progress.checked.clone(),
                    highlight: progress.highlight,
                });
            }
            Event::Resize(_, _) => layout = TranscriptLayout::default(),
            _ => {}
        }
    };

    drop(terminal);

    outcome
}
