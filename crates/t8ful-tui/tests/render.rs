use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use t8ful_core::event::AgentEvent;
use t8ful_core::tool::ToolStatus;
use t8ful_tui::state::{AppState, Entry, Header};
use t8ful_tui::ui::{TranscriptLayout, render};

fn state() -> AppState {
    AppState::new(Header {
        model: "openrouter/test-model".to_owned(),
        mode: t8ful_core::policy::Mode::Coding,
        session_id: "session-1".to_owned(),
        project_root: PathBuf::from("/work/project"),
        launch_dir: PathBuf::from("/work/project/sub"),
        version: "0.1.0".to_owned(),
        branch: Some("main".to_owned()),
    })
}

fn draw(app: &mut AppState, width: u16, height: u16) -> String {
    let mut layout = TranscriptLayout::default();

    draw_with_layout(app, &mut layout, width, height)
}

fn draw_with_layout(
    app: &mut AppState,
    layout: &mut TranscriptLayout,
    width: u16,
    height: u16,
) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");

    terminal
        .draw(|frame| render(frame, app, layout))
        .expect("draw succeeds");

    let buffer = terminal.backend().buffer();
    let mut screen = String::new();

    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");

            screen.push_str(symbol);
        }

        screen.push('\n');
    }

    screen
}

#[test]
fn the_footer_shows_the_location_model_mode_and_help_hint() {
    let screen = draw(&mut state(), 120, 30);

    assert!(screen.contains("/work/project/sub (main)"), "{screen}");
    assert!(screen.contains("openrouter/test-model"), "{screen}");
    assert!(screen.contains("coding"), "{screen}");
    assert!(screen.contains("? help"), "{screen}");
    assert!(!screen.contains("ctrl+o expand"), "{screen}");
    assert!(!screen.contains("↑? ↓?"), "{screen}");
}

#[test]
fn a_missing_model_is_named_in_the_footer_and_help() {
    let mut app = state();

    app.header.model.clear();
    app.help = true;

    let screen = draw(&mut app, 120, 30);

    assert_eq!(screen.matches("no model").count(), 2, "{screen}");
}

#[test]
fn a_narrow_footer_drops_the_hint_column() {
    let screen = draw(&mut state(), 40, 20);

    assert!(screen.contains("/work/project/sub (main)"), "{screen}");
    assert!(!screen.contains("openrouter/test-model"), "{screen}");
    assert!(!screen.contains("? help"), "{screen}");
}

#[test]
fn the_usage_line_names_totals_cache_cost_and_context() {
    let mut app = state();

    app.context_limit = Some(1_000_000);
    // The provider's prompt count already includes the cached tokens, so the
    // cache read has to be a subset of the input: the hit rate is a share of it
    // and the context is that prompt plus the output.
    app.apply_event(AgentEvent::UsageRecorded {
        request_id: "request-1".to_owned(),
        model: "openrouter/test-model".to_owned(),
        agent: "root".to_owned(),
        kind: "turn".to_owned(),
        input_tokens: Some(143_000),
        output_tokens: Some(1_700),
        cost_usd: Some("0.0031".to_owned()),
        generation_id: None,
        cache_read_tokens: Some(86_400),
        cache_write_tokens: Some(13_600),
        reasoning_tokens: Some(500),
    });

    let screen = draw(&mut app, 120, 30);

    assert!(
        screen.contains("↑143k ↓1.7k CH60.4% $0.003 14.5%/1.0M"),
        "{screen}"
    );
    assert!(!screen.contains("reasoning 500"), "{screen}");
    assert!(!screen.contains("(auto)"), "{screen}");
}

#[test]
fn agent_state_never_appears_in_the_footer() {
    let mut app = state();

    app.usage.input_tokens = Some(10);
    app.set_status("running Bash");
    let screen = draw(&mut app, 120, 30);

    assert!(!screen.contains("running Bash"), "{screen}");
    assert!(screen.contains("↑10"), "{screen}");
}

#[test]
fn an_unknown_context_renders_as_a_question_mark() {
    let mut app = state();

    app.context_limit = Some(1_000_000);
    app.usage.context_used = None;

    let screen = draw(&mut app, 120, 30);

    assert!(screen.contains("?/1.0M"), "{screen}");
}

#[test]
fn a_fresh_session_shows_the_banner_and_a_resumed_one_does_not() {
    let mut app = state();

    app.apply_event(AgentEvent::HistoryRestored {
        entries: Vec::new(),
    });

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("t8ful v0.1.0"), "{screen}");
    assert!(screen.contains("escape interrupt"), "{screen}");
    assert!(screen.contains("tab plan/code"), "{screen}");
    assert!(screen.contains("ctrl+o expand tool output"), "{screen}");
    assert!(screen.contains("Thoughts matter."), "{screen}");

    let mut resumed = state();

    resumed.apply_event(AgentEvent::HistoryRestored {
        entries: vec![t8ful_core::event::HistoryEntry {
            arguments: None,
            role: t8ful_core::event::HistoryRole::User,
            images: Vec::new(),
            text: "RESTORED-PROMPT".to_owned(),
            tool: None,
            status: None,
            truncated: false,
        }],
    });

    let screen = draw(&mut resumed, 100, 30);

    assert!(screen.contains("RESTORED-PROMPT"), "{screen}");
    assert!(!screen.contains("Thoughts matter."), "{screen}");
}

#[test]
fn the_viewport_stays_anchored_when_content_arrives_while_scrolled_up() {
    let mut app = state();
    let mut layout = TranscriptLayout::default();

    app.entries = (1..=40)
        .map(|index| Entry::Info(format!("line {index:02}")))
        .collect();

    let _ = draw_with_layout(&mut app, &mut layout, 60, 12);

    // Scroll to the top of the transcript.
    app.scroll = usize::MAX;

    let top = draw_with_layout(&mut app, &mut layout, 60, 12);

    assert!(top.contains("line 01"), "{top}");

    app.entries.push(Entry::Info("line 41".to_owned()));

    let after = draw_with_layout(&mut app, &mut layout, 60, 12);

    assert!(after.contains("line 01"), "the viewport moved: {after}");

    // Following the tail still tracks new content.
    app.scroll = 0;

    let tail = draw_with_layout(&mut app, &mut layout, 60, 12);

    assert!(tail.contains("line 41"), "{tail}");
}

#[test]
fn toggling_reasoning_keeps_a_scrolled_viewport_anchored() {
    let mut app = state();
    let mut layout = TranscriptLayout::default();

    app.entries = (1..=24)
        .map(|index| Entry::Info(format!("line {index:02}")))
        .collect();
    app.entries.insert(
        6,
        Entry::Reasoning("REASONING-ONE\nREASONING-TWO".to_owned()),
    );

    let _ = draw_with_layout(&mut app, &mut layout, 60, 20);

    let height = app.transcript_height;
    // Put the first row on a committed line *after* the reasoning block, so
    // hiding it really moves the viewport.
    let target = 22;

    app.scroll = layout
        .total_lines()
        .saturating_sub(height)
        .saturating_sub(target);

    let _ = draw_with_layout(&mut app, &mut layout, 60, 20);

    let top = layout
        .line_text(app.transcript_start)
        .expect("a top line before the toggle");

    assert!(top.contains("line 12"), "{top}");

    let hidden_total = layout.total_lines();

    app.toggle_reasoning();

    let _ = draw_with_layout(&mut app, &mut layout, 60, 20);
    let after_show = layout
        .line_text(app.transcript_start)
        .expect("a top line after showing reasoning");

    assert!(
        layout.total_lines() > hidden_total,
        "the reasoning block did not appear"
    );
    assert_eq!(
        after_show, top,
        "the viewport jumped when reasoning appeared"
    );

    app.toggle_reasoning();

    let _ = draw_with_layout(&mut app, &mut layout, 60, 20);
    let after_hide = layout
        .line_text(app.transcript_start)
        .expect("a top line after hiding reasoning");

    assert_eq!(
        layout.total_lines(),
        hidden_total,
        "the reasoning block did not disappear"
    );
    assert_eq!(
        after_hide, top,
        "the viewport jumped when reasoning disappeared"
    );
}

#[test]
fn plan_mode_is_shown_in_the_footer_and_nowhere_above_the_input() {
    let mut app = state();

    app.apply_event(AgentEvent::ModeChanged {
        mode: t8ful_core::policy::Mode::Plan,
        previous: None,
    });

    let screen = draw(&mut app, 100, 30);
    let lower = screen.to_ascii_lowercase();

    assert!(lower.contains("plan"), "{screen}");
    assert!(!screen.contains("plan mode"), "{screen}");
    assert!(!screen.contains("plan mode banner"), "{screen}");
}

#[test]
fn renders_streaming_text_and_finished_messages() {
    let mut app = state();

    app.apply_event(AgentEvent::TextDelta {
        text: "streaming reply".to_owned(),
    });

    let screen = draw(&mut app, 80, 24);

    assert!(screen.contains("streaming reply"), "{screen}");

    app.apply_event(AgentEvent::TextCompleted {
        text: "final reply".to_owned(),
    });

    let screen = draw(&mut app, 80, 24);

    assert!(screen.contains("final reply"), "{screen}");
}

#[test]
fn renders_tool_headers_with_status_titles_and_output() {
    let mut app = state();

    app.apply_event(AgentEvent::ToolCallStarted {
        call_id: "call-1".to_owned(),
        tool: "read_file".to_owned(),
        arguments: json!({ "path": "src/lib.rs" }),
    });
    app.apply_event(AgentEvent::ToolResult {
        call_id: "call-1".to_owned(),
        tool: "read_file".to_owned(),
        status: ToolStatus::Ok,
        content: "file contents".to_owned(),
        truncated: true,
        process_id: None,
    });

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("✓ Read src/lib.rs"), "{screen}");
    assert!(screen.contains("file contents"), "{screen}");
}

#[test]
fn a_recall_card_shows_the_query_and_no_model_wrapper() {
    let mut app = state();
    let content = "<recall scope=\"project\">\n2 matching messages from 22 sessions. Only \
                  prompts and assistant replies are searched.\n\n- abc · assistant · 4d ago · \
                  coding · /work/project\n  title: Investigate\n  Let me confirm the \
                  «steering» «recall» path.\n\nResume one with `t8ful --resume <id>`.\n</recall>";

    app.apply_event(AgentEvent::ToolCallStarted {
        call_id: "call-1".to_owned(),
        tool: "recall".to_owned(),
        arguments: json!({ "query": "steering recall" }),
    });
    app.apply_event(AgentEvent::ToolResult {
        call_id: "call-1".to_owned(),
        tool: "recall".to_owned(),
        status: ToolStatus::Ok,
        content: content.to_owned(),
        truncated: false,
        process_id: None,
    });

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("✓ Recall \"steering recall\""), "{screen}");
    assert!(!screen.contains("<recall"), "{screen}");
    assert!(screen.contains("scope: project"), "{screen}");
    assert!(screen.contains("title: Investigate"), "{screen}");

    app.toggle_expanded();

    let expanded = draw(&mut app, 100, 30);

    assert!(expanded.contains("Resume one with"), "{expanded}");
}

#[test]
fn long_tool_output_collapses_and_ctrl_o_expands_it() {
    let mut app = state();
    let body = std::iter::once("FIRST-TOOL-LINE".to_owned())
        .chain((1..=200).map(|line| format!("tool line {line}")))
        .chain(std::iter::once("FINAL-TOOL-LINE".to_owned()))
        .collect::<Vec<_>>()
        .join("\n");

    app.apply_event(AgentEvent::ToolResult {
        call_id: "call-1".to_owned(),
        tool: "bash".to_owned(),
        status: ToolStatus::Ok,
        content: body,
        truncated: false,
        process_id: None,
    });

    let collapsed = draw(&mut app, 100, 30);

    assert!(collapsed.contains("FINAL-TOOL-LINE"), "{collapsed}");
    assert!(
        collapsed.contains("earlier lines, ctrl+o to expand"),
        "{collapsed}"
    );
    assert!(!collapsed.contains("FIRST-TOOL-LINE"), "{collapsed}");

    app.toggle_expanded();

    let expanded_max = {
        let _ = draw(&mut app, 100, 30);

        app.max_scroll
    };

    app.scroll = usize::MAX;

    let expanded = draw(&mut app, 100, 30);

    assert!(expanded.contains("FIRST-TOOL-LINE"), "{expanded}");
    assert!(expanded_max > 0);
}

#[test]
fn a_failed_bash_result_shows_the_error_glyph_and_every_line() {
    let mut app = state();
    let body = std::iter::once("[exit code 1, 0.2s]".to_owned())
        .chain((1..=30).map(|line| format!("error line {line}")))
        .collect::<Vec<_>>()
        .join("\n");

    app.apply_event(AgentEvent::ToolResult {
        call_id: "call-1".to_owned(),
        tool: "bash".to_owned(),
        status: ToolStatus::Ok,
        content: body,
        truncated: false,
        process_id: None,
    });

    let screen = draw(&mut app, 100, 40);

    assert!(screen.contains("✗ Bash"), "{screen}");
    assert!(!screen.contains("earlier lines"), "{screen}");
}

#[test]
fn a_long_bash_command_wraps_in_the_card_header() {
    let mut app = state();

    app.apply_event(AgentEvent::ToolCallStarted {
        call_id: "call-1".to_owned(),
        tool: "bash".to_owned(),
        arguments: json!({
            "command": "cargo test --workspace --all-targets --all-features -- --nocapture"
        }),
    });
    app.apply_event(AgentEvent::ToolResult {
        call_id: "call-1".to_owned(),
        tool: "bash".to_owned(),
        status: ToolStatus::Ok,
        content: "done".to_owned(),
        truncated: false,
        process_id: None,
    });

    let screen = draw(&mut app, 60, 20);
    let squeezed = screen.split_whitespace().collect::<String>();

    assert!(screen.contains("Bash cargo test"), "{screen}");
    assert!(squeezed.contains("--all-features"), "{screen}");
    assert!(squeezed.contains("--nocapture"), "{screen}");
    assert!(!screen.contains('…'), "{screen}");
}

#[test]
fn a_denied_tool_result_renders_in_full_and_is_not_collapsed() {
    let mut app = state();
    let body = (1..=50)
        .map(|line| format!("denied line {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    app.apply_event(AgentEvent::ToolResult {
        call_id: "call-1".to_owned(),
        tool: "edit_file".to_owned(),
        status: ToolStatus::Denied,
        content: body,
        truncated: false,
        process_id: None,
    });

    app.scroll = usize::MAX;

    let screen = draw(&mut app, 100, 40);

    assert!(screen.contains("⊘ Edit"), "{screen}");
    assert!(!screen.contains("earlier lines"), "{screen}");
}

#[test]
fn reasoning_is_hidden_by_default_and_ctrl_r_shows_it() {
    let mut app = state();

    app.entries
        .push(Entry::Reasoning("PRIVATE-REASONING".to_owned()));

    let hidden = draw(&mut app, 100, 30);

    assert!(!hidden.contains("PRIVATE-REASONING"), "{hidden}");

    app.toggle_reasoning();

    let shown = draw(&mut app, 100, 30);

    assert!(shown.contains("PRIVATE-REASONING"), "{shown}");
}

#[test]
fn a_long_tool_result_fits_a_narrow_terminal_viewport() {
    let mut app = state();
    let body = (1..=300)
        .map(|line| format!("line {line}"))
        .chain(std::iter::once("NARROW-FINAL-SENTINEL".to_owned()))
        .collect::<Vec<_>>()
        .join("\n");

    app.apply_event(AgentEvent::ToolResult {
        call_id: "call-1".to_owned(),
        tool: "bash".to_owned(),
        status: ToolStatus::Ok,
        content: body,
        truncated: false,
        process_id: None,
    });

    let screen = draw(&mut app, 40, 18);

    assert!(screen.contains("NARROW-FINAL-SENTINEL"), "{screen}");
}

#[test]
fn resize_redraws_without_panicking() {
    let mut app = state();

    app.apply_event(AgentEvent::TextCompleted {
        text: "a fairly long answer that needs wrapping at narrow widths".to_owned(),
    });

    for (width, height) in [(120, 40), (80, 24), (40, 15), (200, 60)] {
        let screen = draw(&mut app, width, height);

        assert!(!screen.is_empty(), "no render at {width}x{height}");
    }
}

#[test]
fn error_entries_are_visible() {
    let mut app = state();

    app.apply_event(AgentEvent::Error {
        message: "provider authentication failed".to_owned(),
    });

    let screen = draw(&mut app, 90, 24);

    assert!(
        screen.contains("error: provider authentication failed"),
        "{screen}"
    );
}

#[test]
fn page_scrolling_moves_one_viewport_and_reverses_immediately() {
    let mut app = state();
    let body = (0..200)
        .map(|line| format!("page line {line}"))
        .collect::<Vec<_>>()
        .join("\n\n");

    app.entries.push(Entry::Assistant(body));

    let _ = draw(&mut app, 80, 24);

    let page = app.transcript_height;

    assert!(page > 1, "the transcript should have a usable page");
    assert_eq!(app.scroll, 0);

    app.scroll_up(page);
    assert_eq!(app.scroll, page);

    for _ in 0..100 {
        app.scroll_up(page);
    }

    assert_eq!(app.scroll, app.max_scroll);

    app.scroll_down(1);

    assert_eq!(app.scroll, app.max_scroll - 1);

    app.scroll_down(page * 100);

    assert_eq!(app.scroll, 0);
}

#[test]
fn resizing_reflows_and_clamps_a_stale_scroll_offset() {
    let mut app = state();
    let body = (0..200)
        .map(|line| format!("resize line {line}"))
        .collect::<Vec<_>>()
        .join("\n\n");

    app.entries.push(Entry::Assistant(body));
    let _ = draw(&mut app, 120, 40);
    app.scroll = app.max_scroll;

    let wide_max = app.max_scroll;
    let _ = draw(&mut app, 80, 18);

    assert!(app.scroll <= app.max_scroll);
    assert_ne!(app.max_scroll, wide_max);
}

#[test]
fn the_help_overlay_lists_keys_and_commands() {
    let mut app = state();

    app.help = true;

    // Every command row must remain visible; the table grew with `/skill`.
    let screen = draw(&mut app, 100, 36);

    assert!(screen.contains("location"), "{screen}");
    assert!(screen.contains("session session-1"), "{screen}");
    assert!(screen.contains("keys"), "{screen}");
    assert!(screen.contains("commands"), "{screen}");
    assert!(screen.contains("Ctrl-O expand tool output"), "{screen}");
    assert!(screen.contains("Ctrl-R reasoning"), "{screen}");
    assert!(screen.contains("/reasoning"), "{screen}");
    assert!(screen.contains("mode: coding"), "{screen}");
    assert!(screen.contains("? or Esc closes"), "{screen}");
}

#[test]
fn the_help_overlay_uses_the_terminal_background() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut app = state();
    let mut layout = TranscriptLayout::default();

    app.help = true;

    terminal
        .draw(|frame| render(frame, &mut app, &mut layout))
        .expect("draw succeeds");

    let buffer = terminal.backend().buffer();
    let overlay = t8ful_tui::Theme::pi().overlay;

    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            assert_ne!(
                buffer.cell((x, y)).map(|cell| cell.bg),
                Some(overlay),
                "the help overlay painted a background at ({x}, {y})"
            );
        }
    }
}

#[test]
fn queued_follow_ups_render_above_the_composer() {
    let mut app = state();

    let quiet = draw(&mut app, 100, 30);

    assert!(!quiet.contains("Follow-up"), "{quiet}");

    app.queue = vec!["follow up".to_owned()];
    app.queue_paused = true;

    let busy = draw(&mut app, 100, 30);

    assert!(busy.contains("Follow-up: follow up"), "{busy}");
    assert!(
        busy.contains("held · ↳ alt+up to edit pending messages"),
        "{busy}"
    );
}

#[test]
fn pending_steering_renders_as_unsent_input_above_follow_ups() {
    let mut app = state();

    app.remember_steering("check the failing test".to_owned());
    app.queue = vec!["after that".to_owned()];

    let screen = draw(&mut app, 100, 30);

    assert!(
        screen.contains("Steering: check the failing test"),
        "{screen}"
    );
    assert!(screen.contains("Follow-up: after that"), "{screen}");
    assert!(
        screen.contains("alt+up to edit pending messages"),
        "{screen}"
    );
    // The unsent steer is not a transcript message.
    assert!(!screen.contains("you  check"), "{screen}");

    // Delivery replaces the pending line with exactly one user entry.
    app.apply_event(AgentEvent::UserMessage {
        text: "check the failing test".to_owned(),
        source: t8ful_core::event::UserMessageSource::Steered,
        images: Vec::new(),
    });

    let delivered = draw(&mut app, 100, 30);

    assert!(!delivered.contains("Steering:"), "{delivered}");
    assert_eq!(
        delivered.matches("check the failing test").count(),
        1,
        "{delivered}"
    );
}

#[test]
fn a_steering_hint_is_not_shown_when_only_follow_ups_are_queued() {
    let mut app = state();

    app.queue = vec!["later".to_owned()];

    let screen = draw(&mut app, 100, 30);

    assert!(!screen.contains("Steering:"), "{screen}");
    assert!(screen.contains("Follow-up: later"), "{screen}");
}

#[test]
fn at_most_three_follow_ups_render_verbatim_and_the_rest_are_counted() {
    let mut app = state();

    app.queue = (1..=5).map(|index| format!("item {index}")).collect();

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("Follow-up: item 1"), "{screen}");
    assert!(screen.contains("Follow-up: item 3"), "{screen}");
    assert!(!screen.contains("Follow-up: item 4"), "{screen}");
    assert!(
        screen.contains("+2 more · ↳ alt+up to edit pending messages"),
        "{screen}"
    );
}

#[test]
fn a_narrow_follow_up_line_is_elided_but_keeps_its_hint() {
    let mut app = state();

    app.queue = vec!["a very long queued message that will not fit".to_owned()];

    let screen = draw(&mut app, 40, 20);

    assert!(screen.contains("Follow-up: a very long"), "{screen}");
    assert!(screen.contains("↳ alt+up"), "{screen}");
    assert!(!screen.contains("will not fit"), "{screen}");
}

#[test]
fn completion_popup_lists_matching_commands_above_the_composer() {
    let mut app = state();

    app.set_input("/re".to_owned());
    app.refresh_completions();

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("/reasoning"), "{screen}");
    assert!(screen.contains("/resume"), "{screen}");
    assert!(screen.contains("Tab complete"), "{screen}");
    assert!(screen.contains("Enter run"), "{screen}");
}

#[test]
fn completion_rows_show_their_argument_hint() {
    let mut app = state();

    app.set_input("/re".to_owned());
    app.refresh_completions();

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("<level>"), "{screen}");
    assert!(screen.contains("<session-id>"), "{screen}");

    app.set_input("/reasoning ".to_owned());
    app.refresh_completions();

    let levels = draw(&mut app, 100, 30);

    assert!(levels.contains("/reasoning default"), "{levels}");
    assert!(levels.contains("/reasoning high"), "{levels}");
    assert!(levels.contains("slower, more deliberation"), "{levels}");
}

#[test]
fn the_login_modal_masks_the_typed_key() {
    let mut app = state();

    app.open_login("openrouter");

    for character in "sk-or-secret".chars() {
        app.login_push(character);
    }

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("login openrouter"), "{screen}");
    assert!(screen.contains("not connected"), "{screen}");
    assert!(
        screen.contains("OPENROUTER_API_KEY overrides a stored key while it is set"),
        "{screen}"
    );
    assert!(screen.contains("••••••••••••"), "{screen}");
    assert!(!screen.contains("secret"), "{screen}");
    assert!(screen.contains("Enter save · Esc cancel"), "{screen}");
}

#[test]
fn the_footer_appends_the_non_default_reasoning_level() {
    let mut app = state();

    app.reasoning_effort = t8ful_core::provider::ReasoningEffort::High;

    let screen = draw(&mut app, 120, 30);

    assert!(screen.contains("reasoning high"), "{screen}");
}

#[test]
fn the_reasoning_picker_lists_only_the_models_levels() {
    let mut app = state();

    // A model with reasoning that offers only low and high.
    app.supports_reasoning = Some(true);
    app.reasoning_levels = vec![
        t8ful_core::provider::ReasoningEffort::Default,
        t8ful_core::provider::ReasoningEffort::Low,
        t8ful_core::provider::ReasoningEffort::High,
    ];
    app.reasoning_picker = Some(1);

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("reasoning level"), "{screen}");
    assert!(screen.contains("> low"), "{screen}");
    assert!(screen.contains("high"), "{screen}");
    assert!(
        !screen.contains("minimal") && !screen.contains("xhigh"),
        "the picker offered a level the model does not have: {screen}"
    );
}

#[test]
fn the_reasoning_picker_marks_unsupported_models() {
    let mut app = state();

    app.supports_reasoning = Some(false);
    app.reasoning_levels = vec![
        t8ful_core::provider::ReasoningEffort::Default,
        t8ful_core::provider::ReasoningEffort::Off,
    ];
    app.reasoning_picker = Some(1);

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("reasoning level"), "{screen}");
    assert!(screen.contains("off"), "{screen}");
    assert!(
        !screen.contains("high"),
        "a model without reasoning offered high: {screen}"
    );
    assert!(
        screen.contains("this model does not advertise reasoning"),
        "{screen}"
    );
}

#[test]
fn a_finished_process_leaves_the_notice() {
    let mut app = state();

    app.entries
        .push(Entry::Info("PROCESS-RESULT-KEPT".to_owned()));
    app.apply_event(AgentEvent::ProcessSnapshot {
        entries: vec!["process-1 running sleep 60".to_owned()],
    });

    let running = draw(&mut app, 100, 30);

    assert!(running.contains("1 process running"), "{running}");

    app.apply_event(AgentEvent::ProcessStatus {
        process_id: "process-1".to_owned(),
        state: "exited".to_owned(),
        exit_code: Some(0),
    });

    let finished = draw(&mut app, 100, 30);

    assert!(!finished.contains("process running"), "{finished}");
    assert!(!finished.contains("1 process"), "{finished}");
    assert!(finished.contains("PROCESS-RESULT-KEPT"), "{finished}");
}

#[test]
fn the_help_overlay_is_generated_from_the_command_table() {
    let mut app = state();

    app.help = true;

    let screen = draw(&mut app, 100, 40);

    assert!(screen.contains("/show-reasoning"), "{screen}");
    assert!(screen.contains("show or hide reasoning text"), "{screen}");
    assert!(screen.contains("Alt-E reasoning level"), "{screen}");
    assert!(!screen.contains("approve"), "{screen}");
}

#[test]
fn the_question_panel_replaces_the_composer_and_marks_the_highlighted_row() {
    use t8ful_core::question::{Question, QuestionOption, QuestionRequest};
    use t8ful_tui::state::QuestionProgress;

    let mut app = state();

    app.question = Some(QuestionProgress::begin(QuestionRequest {
        questions: vec![Question {
            header: "Scope".to_owned(),
            question: "Which file?".to_owned(),
            options: vec![
                QuestionOption {
                    label: "a.rs".to_owned(),
                    description: "the first one".to_owned(),
                    preview: None,
                },
                QuestionOption {
                    label: "b.rs".to_owned(),
                    description: "the second one".to_owned(),
                    preview: None,
                },
            ],
            multi_select: false,
        }],
    }));

    if let Some(progress) = &mut app.question {
        progress.move_highlight(true);
    }

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("→ 2. b.rs  – the second one"), "{screen}");
    assert!(screen.contains("Type something."), "{screen}");
    assert!(
        screen.contains("↑↓ navigate · enter select · 1-4 select"),
        "{screen}"
    );
    assert!(!screen.contains("Review"), "{screen}");
    assert!(!screen.contains("question 1/"), "{screen}");
}

#[test]
fn question_tabs_end_on_a_review_step() {
    use t8ful_core::question::{Question, QuestionOption, QuestionRequest};
    use t8ful_tui::state::QuestionProgress;

    let mut app = state();
    let request = QuestionRequest {
        questions: vec![
            Question {
                header: "Scope".to_owned(),
                question: "Which file?".to_owned(),
                options: vec![
                    QuestionOption {
                        label: "a.rs".to_owned(),
                        description: String::new(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "b.rs".to_owned(),
                        description: String::new(),
                        preview: None,
                    },
                ],
                multi_select: false,
            },
            Question {
                header: "Tests".to_owned(),
                question: "Which suite?".to_owned(),
                options: vec![
                    QuestionOption {
                        label: "Fast".to_owned(),
                        description: String::new(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "Full".to_owned(),
                        description: String::new(),
                        preview: None,
                    },
                ],
                multi_select: false,
            },
        ],
    };

    app.question = Some(QuestionProgress::begin(request));

    let question = draw(&mut app, 100, 30);

    assert!(question.contains("[Scope] Tests Review"), "{question}");

    if let Some(progress) = &mut app.question {
        progress.choose(0);
        progress.choose(1);
    }

    let review = draw(&mut app, 100, 30);

    assert!(review.contains("Scope Tests [Review]"), "{review}");
    assert!(review.contains("Scope: a.rs"), "{review}");
    assert!(review.contains("Tests: Full"), "{review}");
    assert!(review.contains("enter submit"), "{review}");
}

#[test]
fn a_single_select_preview_sits_beside_the_options() {
    use t8ful_core::question::{Question, QuestionOption, QuestionRequest};
    use t8ful_tui::state::QuestionProgress;

    let mut app = state();

    app.question = Some(QuestionProgress::begin(QuestionRequest {
        questions: vec![Question {
            header: "Scope".to_owned(),
            question: "Which file?".to_owned(),
            options: vec![QuestionOption {
                label: "a.rs".to_owned(),
                description: "the first one".to_owned(),
                preview: Some("# Preview\n\nthe option details".to_owned()),
            }],
            multi_select: false,
        }],
    }));

    let wide = draw(&mut app, 120, 30);

    assert!(wide.contains("Preview"), "{wide}");
    assert!(wide.contains("the option details"), "{wide}");
    assert!(wide.contains('│'), "{wide}");

    let narrow = draw(&mut app, 60, 30);

    assert!(!narrow.contains("Preview"), "{narrow}");
    assert!(!narrow.contains('│'), "{narrow}");
}

#[test]
fn multi_select_shows_boxes_and_a_done_row_without_previews() {
    use t8ful_core::question::{Question, QuestionOption, QuestionRequest};
    use t8ful_tui::state::QuestionProgress;

    let mut app = state();

    app.question = Some(QuestionProgress::begin(QuestionRequest {
        questions: vec![Question {
            header: "Scope".to_owned(),
            question: "Which files?".to_owned(),
            options: vec![
                QuestionOption {
                    label: "a.rs".to_owned(),
                    description: String::new(),
                    preview: Some("# Hidden".to_owned()),
                },
                QuestionOption {
                    label: "b.rs".to_owned(),
                    description: String::new(),
                    preview: None,
                },
            ],
            multi_select: true,
        }],
    }));

    if let Some(progress) = &mut app.question {
        progress.toggle_option(0);
    }

    let screen = draw(&mut app, 120, 30);

    assert!(screen.contains("→ 1. [✓] a.rs"), "{screen}");
    assert!(screen.contains("2. [ ] b.rs"), "{screen}");
    assert!(screen.contains("Done"), "{screen}");
    assert!(screen.contains("enter or space toggle"), "{screen}");
    assert!(!screen.contains("Hidden"), "{screen}");
}

#[test]
fn a_custom_answer_opens_an_inline_editor() {
    use t8ful_core::question::{Question, QuestionOption, QuestionRequest};
    use t8ful_tui::state::QuestionProgress;

    let mut app = state();

    app.question = Some(QuestionProgress::begin(QuestionRequest {
        questions: vec![Question {
            header: "Scope".to_owned(),
            question: "Which file?".to_owned(),
            options: vec![QuestionOption {
                label: "a.rs".to_owned(),
                description: String::new(),
                preview: None,
            }],
            multi_select: false,
        }],
    }));

    if let Some(progress) = &mut app.question {
        progress.toggle_custom();
        progress.custom.set_text("hand written");
    }

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("Your answer:"), "{screen}");
    assert!(screen.contains("hand written"), "{screen}");
    assert!(screen.contains("enter submit · esc back"), "{screen}");
}

#[test]
fn the_plan_hand_off_uses_the_inline_panel() {
    use t8ful_tui::state::PlanChoice;

    let mut app = state();

    app.plan_choice = Some(PlanChoice {
        plan: "# Ship it".to_owned(),
        highlight: 1,
    });

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("Plan ready"), "{screen}");
    assert!(
        screen.contains("→ 2. Implement in a new session  – keep this session untouched"),
        "{screen}"
    );
    assert!(screen.contains("esc stay in plan mode"), "{screen}");
}

fn draw_buffer(app: &mut AppState, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut layout = TranscriptLayout::default();

    terminal
        .draw(|frame| render(frame, app, &mut layout))
        .expect("draw succeeds");

    terminal.backend().buffer().clone()
}

fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
        .collect()
}

fn screen_text(buffer: &ratatui::buffer::Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_presented_plan_renders_as_one_marked_markdown_block() {
    let mut app = state();
    let plan = "# Ship it\n\nIntro paragraph.\n\n- first step\n- second step\n\n\
                ```rust\nfn main() {}\n```\n\nDone.\n";

    app.entries.push(Entry::Plan {
        body: plan.to_owned(),
        mode: t8ful_core::policy::Mode::Plan,
        placement: Default::default(),
    });

    let buffer = draw_buffer(&mut app, 100, 40);
    let theme = t8ful_tui::Theme::pi();
    let screen = screen_text(&buffer);
    let mut header = None;
    let mut heading = None;
    let mut code = None;

    for y in 0..buffer.area.height {
        let row = row_text(&buffer, y);

        if row.contains("· presented") {
            header = Some(y);
        }

        if row.contains("Ship it") {
            heading = Some(y);
        }

        if row.contains("fn main") {
            code = Some(y);
        }
    }

    assert!(screen.contains("▏Plan"), "{screen}");
    assert!(screen.contains("· presented"), "{screen}");
    assert!(screen.contains("▌ Ship it"), "{screen}");
    assert!(!screen.contains("# Ship it"), "{screen}");
    assert!(screen.contains("• first step"), "{screen}");
    assert!(screen.contains("fn main() {}"), "{screen}");
    assert!(!screen.contains("```"), "{screen}");
    assert!(
        screen.contains("proposal · reply with changes, or tab to switch"),
        "{screen}"
    );

    let header = header.expect("plan header row");

    assert_eq!(
        buffer.cell((0, header)).map(|cell| cell.fg),
        Some(theme.warning)
    );
    assert_eq!(
        buffer.cell((0, header)).map(|cell| cell.bg),
        theme.tool_bg,
        "the plan panel should paint its band"
    );

    let heading = heading.expect("heading row");

    // The heading's first character sits after the block gutter, a space, and
    // the markdown heading marker.
    assert_eq!(
        buffer.cell((0, heading)).map(|cell| cell.fg),
        Some(theme.warning)
    );
    assert_eq!(
        buffer.cell((4, heading)).map(|cell| cell.fg),
        Some(theme.accent)
    );

    let code = code.expect("code row");

    assert_eq!(
        buffer.cell((3, code)).map(|cell| cell.fg),
        Some(theme.title),
        "the `fn` keyword should be syntax highlighted"
    );
}

#[test]
fn a_gutter_only_theme_still_draws_the_plan_panel_boundary() {
    let mut app = state();

    app.theme = t8ful_tui::Theme::ansi();
    app.entries.push(Entry::Plan {
        body: "# A plan".to_owned(),
        mode: t8ful_core::policy::Mode::Plan,
        placement: Default::default(),
    });

    let screen = draw(&mut app, 80, 24);

    assert!(screen.contains("▏Plan"), "{screen}");
    assert!(
        screen.contains("proposal · reply with changes, or tab to switch"),
        "{screen}"
    );
}

#[test]
fn a_long_plan_is_never_collapsed_or_truncated() {
    let mut app = state();
    let body = std::iter::once("PLAN-START".to_owned())
        .chain((1..=60).map(|line| format!("step {line}")))
        .chain(std::iter::once("PLAN-END".to_owned()))
        .collect::<Vec<_>>()
        .join("\n\n");

    app.entries.push(Entry::Plan {
        body,
        mode: t8ful_core::policy::Mode::Plan,
        placement: Default::default(),
    });

    let bottom = draw(&mut app, 100, 30);

    assert!(bottom.contains("PLAN-END"), "{bottom}");
    assert!(!bottom.contains("ctrl+o to expand"), "{bottom}");
    assert!(!bottom.contains("earlier lines"), "{bottom}");
    assert!(!bottom.contains("more lines"), "{bottom}");

    app.scroll = usize::MAX;

    let top = draw(&mut app, 100, 30);

    assert!(top.contains("PLAN-START"), "{top}");
    assert!(!top.contains("PLAN-END"), "{top}");
}

#[test]
fn a_divider_narrower_than_its_label_still_renders() {
    let mut app = state();

    app.entries.push(Entry::ModeChange {
        from: t8ful_core::policy::Mode::Coding,
        to: t8ful_core::policy::Mode::Plan,
    });

    let screen = draw(&mut app, 12, 20);

    assert!(screen.contains("coding"), "{screen}");
    assert!(screen.contains('→'), "{screen}");
}

fn summary(id: &str, title: &str, last_message_at: u64) -> t8ful_tui::bridge::SessionSummary {
    t8ful_tui::bridge::SessionSummary {
        id: id.to_owned(),
        title: title.to_owned(),
        last_message_at,
        project: "project".to_owned(),
        resumable: true,
    }
}

#[test]
fn the_model_picker_filters_and_reports_an_empty_result() {
    let mut app = state();

    app.models = vec![
        "vendor/large".to_owned(),
        "vendor/small".to_owned(),
        "other/model".to_owned(),
    ];
    app.picker = Some(0);
    app.picker_push_char('l');
    app.picker_push_char('r');
    app.picker_push_char('g');

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("model picker"), "{screen}");
    assert!(screen.contains("filter: lrg"), "{screen}");
    assert!(screen.contains("vendor/large"), "{screen}");
    assert!(!screen.contains("vendor/small"), "{screen}");
    assert!(screen.contains("type to filter"), "{screen}");
    assert!(screen.contains("Enter apply at the next tur"), "{screen}");

    let mut empty = state();

    empty.models = vec!["vendor/large".to_owned()];
    empty.picker = Some(0);
    empty.picker_push_char('z');

    let screen = draw(&mut empty, 100, 30);

    assert!(screen.contains("filter: z"), "{screen}");
    assert!(screen.contains("no matching models"), "{screen}");
    assert!(!screen.contains("vendor/large"), "{screen}");
}

#[test]
fn the_session_picker_lists_titles_ages_and_orders_by_recency() {
    let mut app = state();
    let now = t8ful_tui::now_seconds();

    // Published oldest-message-first on purpose: the picker orders by recency,
    // not by the order the snapshot happened to use.
    app.apply_sessions(t8ful_tui::bridge::SessionSnapshot::Loaded {
        sessions: vec![
            summary("session-stale", "older work", 10),
            summary("session-fresh", "newer work", 30),
            summary("session-1", "current session", 10),
            summary("session-recent", "just started", now - 30),
            summary("session-older", "left running", now - 2 * 3_600),
        ],
    });
    app.open_session_picker();

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("resume session"), "{screen}");
    assert!(screen.contains("filter: all sessions"), "{screen}");
    assert!(screen.contains("session-fresh"), "{screen}");
    assert!(screen.contains("session-stale"), "{screen}");
    assert!(screen.contains("older work"), "{screen}");
    assert!(screen.contains("newer work"), "{screen}");
    assert!(screen.contains("just started"), "{screen}");
    assert!(screen.contains("left running"), "{screen}");
    assert!(screen.contains("just now"), "{screen}");
    assert!(screen.contains("2h"), "{screen}");
    assert!(
        !screen.contains("current session"),
        "the open session is not offered: {screen}"
    );
    assert!(
        screen.contains("type to filter · ↑↓ choose · Enter resume · Esc close"),
        "{screen}"
    );

    let ids = app
        .session_picker_rows()
        .into_iter()
        .map(|session| session.id)
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        vec![
            "session-recent",
            "session-older",
            "session-fresh",
            "session-stale"
        ]
    );
}

#[test]
fn the_session_picker_distinguishes_loading_empty_and_no_match() {
    let mut app = state();

    app.open_session_picker();

    let loading = draw(&mut app, 100, 30);

    assert!(loading.contains("loading sessions…"), "{loading}");

    app.apply_sessions(t8ful_tui::bridge::SessionSnapshot::Loaded {
        sessions: Vec::new(),
    });

    let empty = draw(&mut app, 100, 30);

    assert!(empty.contains("no saved sessions"), "{empty}");

    app.apply_sessions(t8ful_tui::bridge::SessionSnapshot::Loaded {
        sessions: vec![summary("session-alpha", "fix the picker", 30)],
    });

    for character in "zzz".chars() {
        app.session_picker_push_char(character);
    }

    let no_match = draw(&mut app, 100, 30);

    assert!(no_match.contains("filter: zzz"), "{no_match}");
    assert!(no_match.contains("no matching sessions"), "{no_match}");
}

#[test]
fn a_running_child_agent_draws_one_card_with_its_role_and_isolation() {
    let mut app = state();

    app.apply_event(AgentEvent::AgentSpawned {
        agent: "a1".to_owned(),
        role: "researcher".to_owned(),
        task: Some("task-1".to_owned()),
        isolation: "worktree".to_owned(),
        worktree: Some("/state/worktrees/a1".to_owned()),
    });

    let screen = draw(&mut app, 100, 24);

    assert!(screen.contains("Agent a1 · researcher"), "{screen}");
    assert!(screen.contains("running"), "{screen}");
    assert!(screen.contains("worktree"), "{screen}");
    assert!(screen.contains("task-1"), "{screen}");
}

#[test]
fn a_finished_child_agent_draws_its_report_and_the_inspect_command() {
    let mut app = state();

    app.apply_event(AgentEvent::AgentSpawned {
        agent: "a1".to_owned(),
        role: "implementer".to_owned(),
        task: None,
        isolation: "worktree".to_owned(),
        worktree: Some("/state/worktrees/a1".to_owned()),
    });
    app.apply_event(AgentEvent::AgentFinished {
        agent: "a1".to_owned(),
        role: "implementer".to_owned(),
        status: "completed".to_owned(),
        summary: "added the parser and its tests".to_owned(),
        steps: 7,
        task: None,
        worktree: Some("/state/worktrees/a1".to_owned()),
        branch: Some("t8ful/s1/a1".to_owned()),
        elapsed_ms: 4_200,
    });

    let screen = draw(&mut app, 110, 30);

    assert!(screen.contains("completed"), "{screen}");
    assert!(
        screen.contains("added the parser and its tests"),
        "{screen}"
    );
    assert!(
        screen.contains("git -C /state/worktrees/a1 diff"),
        "{screen}"
    );
}

#[test]
fn the_notice_counts_running_agents() {
    let mut app = state();

    app.apply_event(AgentEvent::AgentSpawned {
        agent: "a1".to_owned(),
        role: "general".to_owned(),
        task: None,
        isolation: "shared".to_owned(),
        worktree: None,
    });
    app.apply_event(AgentEvent::TaskChanged {
        id: "task-1".to_owned(),
        title: "schema".to_owned(),
        state: "completed".to_owned(),
        blocked: false,
        owner: None,
    });
    app.apply_event(AgentEvent::TaskChanged {
        id: "task-2".to_owned(),
        title: "parser".to_owned(),
        state: "in_progress".to_owned(),
        blocked: false,
        owner: None,
    });

    let screen = draw(&mut app, 100, 24);

    // The notice counts running agents; task progress lives in the todo block
    // above the composer instead of sharing this line.
    assert!(screen.contains("1 agent running"), "{screen}");
    assert!(screen.contains("Todos (1/2)"), "{screen}");
}

#[test]
fn the_notice_is_absent_when_no_agent_and_no_process_is_running() {
    let mut app = state();

    app.apply_event(AgentEvent::AgentSpawned {
        agent: "a1".to_owned(),
        role: "general".to_owned(),
        task: None,
        isolation: "shared".to_owned(),
        worktree: None,
    });
    app.apply_event(AgentEvent::AgentFinished {
        agent: "a1".to_owned(),
        role: "general".to_owned(),
        status: "completed".to_owned(),
        summary: "done".to_owned(),
        steps: 1,
        task: None,
        worktree: None,
        branch: None,
        elapsed_ms: 10,
    });

    let screen = draw(&mut app, 100, 24);

    assert!(!screen.contains("agent running"), "{screen}");
}

#[test]
fn a_child_event_does_not_add_transcript_entries_to_the_parent() {
    let mut app = state();
    let before = app.entries.len();

    app.apply_event(AgentEvent::AgentSpawned {
        agent: "a1".to_owned(),
        role: "general".to_owned(),
        task: None,
        isolation: "shared".to_owned(),
        worktree: None,
    });

    let after_spawn = app.entries.len();

    app.apply_event(AgentEvent::AgentEvent {
        agent: "a1".to_owned(),
        event: Box::new(AgentEvent::TextDelta {
            text: "child output ".to_owned(),
        }),
    });
    app.apply_event(AgentEvent::AgentEvent {
        agent: "a1".to_owned(),
        event: Box::new(AgentEvent::TextDelta {
            text: "should not appear".to_owned(),
        }),
    });

    // One card for the child, and nothing from its own stream.
    assert_eq!(after_spawn, before + 1);
    assert_eq!(app.entries.len(), after_spawn);
    assert!(!app.streaming.contains("child output"));

    let screen = draw(&mut app, 100, 24);

    assert!(!screen.contains("should not appear"), "{screen}");
}

#[test]
fn a_fresh_state_draws_the_mode_label_at_a_narrow_width() {
    let mut app = state();

    let screen = draw(&mut app, 100, 30);

    assert!(screen.contains("coding"), "{screen}");
}

#[test]
fn the_todo_block_lists_tasks_with_a_progress_header() {
    let mut app = state();

    for (id, title, task_state) in [
        ("task-1", "Set up the toolchain", "completed"),
        ("task-2", "Write the first tests", "completed"),
        ("task-3", "Generate seed data", "in_progress"),
        ("task-4", "Write the reports", "pending"),
    ] {
        app.apply_event(AgentEvent::TaskChanged {
            id: id.to_owned(),
            title: title.to_owned(),
            state: task_state.to_owned(),
            blocked: false,
            owner: None,
        });
    }

    let screen = draw(&mut app, 100, 34);

    assert!(screen.contains("Todos (2/4)"), "{screen}");
    assert!(screen.contains("Set up the toolchain"), "{screen}");
    assert!(screen.contains("Write the first tests"), "{screen}");
    assert!(screen.contains("Generate seed data"), "{screen}");
    assert!(screen.contains("Write the reports"), "{screen}");
    assert!(screen.contains('✓'), "completed rows get a check: {screen}");
    assert!(screen.contains('●'), "the active row gets a dot: {screen}");
    assert!(screen.contains('○'), "pending rows get a ring: {screen}");
}

#[test]
fn a_fully_completed_todo_block_gets_out_of_the_way() {
    let mut app = state();

    app.apply_event(AgentEvent::TaskChanged {
        id: "task-1".to_owned(),
        title: "the only task".to_owned(),
        state: "completed".to_owned(),
        blocked: false,
        owner: None,
    });

    let screen = draw(&mut app, 100, 30);

    assert!(
        !screen.contains("Todos ("),
        "a finished plan should not hold screen space: {screen}"
    );
}

#[test]
fn an_agent_tool_card_shows_the_prompt_it_was_given() {
    let mut app = state();

    app.apply_event(AgentEvent::ToolCallStarted {
        call_id: "call-1".to_owned(),
        tool: "spawn_agents".to_owned(),
        arguments: json!({
            "agents": [{ "prompt": "audit the mutex usage across the workspace" }]
        }),
    });
    app.apply_event(AgentEvent::ToolResult {
        call_id: "call-1".to_owned(),
        tool: "spawn_agents".to_owned(),
        status: ToolStatus::Ok,
        content: "a1 started".to_owned(),
        truncated: false,
        process_id: None,
    });

    let screen = draw(&mut app, 110, 30);

    assert!(
        screen.contains("audit the mutex usage"),
        "the delegated prompt is visible on the card: {screen}"
    );

    // Collapsed is the default; expanding shows the same text with room.
    app.expanded = true;

    let expanded = draw(&mut app, 110, 30);

    assert!(
        expanded.contains("audit the mutex usage"),
        "the prompt survives expansion: {expanded}"
    );
}

#[test]
fn an_image_entry_renders_its_caption_and_a_halfblock_preview() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("preview.png");
    let image = image::RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([(x * 4) as u8, (y * 5) as u8, 160])
    });

    image.save(&path).expect("write fixture");

    let bytes = std::fs::metadata(&path).expect("metadata").len();
    let mut app = state();

    app.entries
        .push(Entry::Images(vec![t8ful_core::content::ImageInput::new(
            "image/png",
            path,
            64,
            48,
            bytes,
        )]));

    let mut layout = TranscriptLayout::default();
    let first = draw_with_layout(&mut app, &mut layout, 80, 30);

    assert!(first.contains("[1] PNG · 64x48"), "{first}");

    for _ in 0..200 {
        if layout.apply_image_results() {
            break;
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let rendered = draw_with_layout(&mut app, &mut layout, 80, 30);

    assert!(rendered.contains("[1] PNG · 64x48"), "{rendered}");
    assert!(
        !rendered.contains("decoding preview"),
        "the pending placeholder was never replaced: {rendered}"
    );
    assert!(
        rendered.chars().any(|c| matches!(c, '▀' | '▄' | '█')),
        "no halfblock preview pixels were drawn: {rendered}"
    );
}

#[test]
fn an_image_caption_stays_readable_on_a_narrow_terminal() {
    let mut app = state();

    app.entries
        .push(Entry::Images(vec![t8ful_core::content::ImageInput::new(
            "image/webp",
            PathBuf::from("/missing/restored.webp"),
            640,
            480,
            2048,
        )]));

    let screen = draw(&mut app, 24, 24);

    assert!(
        screen.contains("PNG") || screen.contains("WEBP"),
        "{screen}"
    );
    assert!(screen.contains("640x480"), "{screen}");
}
