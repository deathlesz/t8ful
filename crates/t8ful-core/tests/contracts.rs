use serde_json::json;
use t8ful_core::event::AgentEvent;
use t8ful_core::policy::Mode;
use t8ful_core::provider::ReasoningEffort;
use t8ful_core::tool::ToolStatus;
use t8ful_core::usage::Usage;

/// The event schema is public: every frontend and the Phase 2 session store
/// depend on these tags and field names staying stable.
#[test]
fn event_schema_is_stable() {
    let events = vec![
        (
            AgentEvent::TurnStarted { turn: 1 },
            json!({ "type": "turn_started", "turn": 1 }),
        ),
        (
            AgentEvent::ModelStarted {
                model: "openrouter/test".to_owned(),
                step: 2,
            },
            json!({ "type": "model_started", "model": "openrouter/test", "step": 2 }),
        ),
        (
            AgentEvent::TextDelta {
                text: "hi".to_owned(),
            },
            json!({ "type": "text_delta", "text": "hi" }),
        ),
        (
            AgentEvent::ToolCallStarted {
                call_id: "call-1".to_owned(),
                tool: "read_file".to_owned(),
                arguments: json!({ "path": "a.txt" }),
            },
            json!({
                "type": "tool_call_started",
                "call_id": "call-1",
                "tool": "read_file",
                "arguments": { "path": "a.txt" }
            }),
        ),
        (
            AgentEvent::ToolResult {
                call_id: "call-1".to_owned(),
                tool: "read_file".to_owned(),
                status: ToolStatus::Denied,
                content: "no".to_owned(),
                truncated: false,
                process_id: None,
                images: Vec::new(),
            },
            json!({
                "type": "tool_result",
                "call_id": "call-1",
                "tool": "read_file",
                "status": "denied",
                "content": "no",
                "truncated": false
            }),
        ),
        (
            AgentEvent::SkillLoaded {
                name: "review".to_owned(),
                content: "instructions".to_owned(),
                truncated: false,
            },
            json!({
                "type": "skill_loaded",
                "name": "review",
                "content": "instructions",
                "truncated": false
            }),
        ),
        (
            AgentEvent::ModelChanged {
                model: "openrouter/test".to_owned(),
                supports_reasoning: Some(true),
            },
            json!({
                "type": "model_changed",
                "model": "openrouter/test",
                "supports_reasoning": true
            }),
        ),
        (
            AgentEvent::ReasoningEffortChanged {
                effort: ReasoningEffort::High,
            },
            json!({ "type": "reasoning_effort_changed", "effort": "high" }),
        ),
        (
            AgentEvent::UsageRecorded {
                request_id: "request-1".to_owned(),
                model: "openrouter/test".to_owned(),
                agent: "root".to_owned(),
                kind: "turn".to_owned(),
                input_tokens: Some(10),
                output_tokens: None,
                cost_usd: None,
                generation_id: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            },
            json!({
                "type": "usage_recorded",
                "request_id": "request-1",
                "model": "openrouter/test",
                "agent": "root",
                "kind": "turn",
                "input_tokens": 10,
                "output_tokens": null,
                "cost_usd": null,
                "generation_id": null
            }),
        ),
        (
            AgentEvent::InputRequired {
                kind: "question".to_owned(),
                detail: "no tty".to_owned(),
            },
            json!({ "type": "input_required", "kind": "question", "detail": "no tty" }),
        ),
        (
            AgentEvent::ModeChanged {
                mode: Mode::Plan,
                previous: Some(Mode::Coding),
            },
            json!({ "type": "mode_changed", "mode": "plan", "previous": "coding" }),
        ),
        (
            AgentEvent::ModeChanged {
                mode: Mode::Coding,
                previous: None,
            },
            json!({ "type": "mode_changed", "mode": "coding", "previous": null }),
        ),
    ];

    for (event, expected) in events {
        let encoded = serde_json::to_value(&event).expect("event serializes");

        assert_eq!(encoded, expected);

        let decoded: AgentEvent = serde_json::from_value(expected).expect("event deserializes");

        assert_eq!(decoded, event);
    }
}

#[test]
fn usage_recorded_serializes_known_cache_and_reasoning_counts() {
    let event = AgentEvent::UsageRecorded {
        request_id: "request-1".to_owned(),
        model: "openrouter/test".to_owned(),
        agent: "root".to_owned(),
        kind: "turn".to_owned(),
        input_tokens: Some(10),
        output_tokens: Some(4),
        cost_usd: None,
        generation_id: None,
        cache_read_tokens: Some(6),
        cache_write_tokens: Some(2),
        reasoning_tokens: Some(3),
    };
    let encoded = serde_json::to_value(&event).expect("event serializes");

    assert_eq!(encoded["cache_read_tokens"], json!(6));
    assert_eq!(encoded["cache_write_tokens"], json!(2));
    assert_eq!(encoded["reasoning_tokens"], json!(3));

    let old = json!({
        "type": "usage_recorded",
        "request_id": "request-1",
        "model": "openrouter/test",
        "input_tokens": 10,
        "output_tokens": 4,
        "cost_usd": null,
        "generation_id": null
    });
    let decoded: AgentEvent = serde_json::from_value(old).expect("old event deserializes");

    assert!(matches!(
        decoded,
        AgentEvent::UsageRecorded {
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            ..
        }
    ));
}

#[test]
fn old_model_changed_events_without_a_capability_still_deserialize() {
    let old = json!({ "type": "model_changed", "model": "openrouter/test" });
    let decoded: AgentEvent = serde_json::from_value(old).expect("old event deserializes");

    assert_eq!(
        decoded,
        AgentEvent::ModelChanged {
            model: "openrouter/test".to_owned(),
            supports_reasoning: None,
        }
    );
}

#[test]
fn old_mode_changed_events_without_a_previous_mode_still_deserialize() {
    let old = json!({ "type": "mode_changed", "mode": "plan" });
    let decoded: AgentEvent = serde_json::from_value(old).expect("old event deserializes");

    assert_eq!(
        decoded,
        AgentEvent::ModeChanged {
            mode: Mode::Plan,
            previous: None,
        }
    );
}

#[test]
fn usage_keeps_unknown_values_unknown() {
    let mut usage = Usage {
        input_tokens: Some(5),
        ..Usage::default()
    };

    assert_eq!(usage.total_tokens(), Some(5));
    assert!(Usage::default().total_tokens().is_none());

    usage.fill_from(&Usage {
        input_tokens: Some(99),
        output_tokens: Some(7),
        ..Usage::default()
    });

    assert_eq!(usage.input_tokens, Some(5));
    assert_eq!(usage.output_tokens, Some(7));
    assert!(usage.cost_usd.is_none());
}

#[test]
fn tool_call_ids_round_trip_as_strings() {
    let call = t8ful_core::tool::ToolCall::new("abc", "read_file", json!({}));
    let encoded = serde_json::to_value(&call).expect("call serializes");

    assert_eq!(encoded["id"], json!("abc"));

    let decoded: t8ful_core::tool::ToolCall =
        serde_json::from_value(encoded).expect("call deserializes");

    assert_eq!(decoded.id.as_str(), "abc");
}

#[test]
fn restored_history_schema_is_stable() {
    use t8ful_core::event::{HistoryEntry, HistoryRole};

    let event = AgentEvent::HistoryRestored {
        entries: vec![
            HistoryEntry {
                arguments: None,
                role: HistoryRole::User,
                images: Vec::new(),
                text: "hello".to_owned(),
                tool: None,
                status: None,
                truncated: false,
            },
            HistoryEntry {
                arguments: None,
                role: HistoryRole::Tool,
                images: Vec::new(),
                text: "result".to_owned(),
                tool: Some("read_file".to_owned()),
                status: Some(ToolStatus::Ok),
                truncated: true,
            },
        ],
    };
    let expected = json!({
        "type": "history_restored",
        "entries": [
            {
                "role": "user",
                "text": "hello",
                "tool": null,
                "status": null,
                "truncated": false
            },
            {
                "role": "tool",
                "text": "result",
                "tool": "read_file",
                "status": "ok",
                "truncated": true
            }
        ]
    });

    assert_eq!(serde_json::to_value(&event).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<AgentEvent>(expected).unwrap(),
        event
    );
}
