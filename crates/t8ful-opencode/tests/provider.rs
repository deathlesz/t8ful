use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use t8ful_core::error::ProviderError;
use t8ful_core::message::Conversation;
use t8ful_core::provider::{
    FinishReason, Provider, ProviderEvent, ProviderRequest, ReasoningEffort,
};
use t8ful_openai::test_support::{FakeServer, Reply, chunked, sse_frame};
use t8ful_opencode::OpenCodeGoClient;

fn request(prompt: &str) -> ProviderRequest {
    request_for(prompt, "ses_conversation")
}

fn request_for(prompt: &str, session: &str) -> ProviderRequest {
    let mut conversation = Conversation::new();

    conversation.push_user_text(prompt);

    ProviderRequest {
        model: "kimi-k3".to_owned(),
        system: "be helpful".to_owned(),
        messages: conversation.messages().to_vec(),
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: ReasoningEffort::Default,
        reasoning_value: None,
        session_id: Some(session.to_owned()),
    }
}

fn text_chunk(text: &str) -> Vec<u8> {
    let value = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "content": text } }]
    });

    sse_frame(&value.to_string()).into_bytes()
}

/// Go streams reasoning as `reasoning_content`; the plain `reasoning` spelling
/// must stay supported for compatibility.
fn reasoning_chunk(text: &str) -> Vec<u8> {
    let value = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "reasoning_content": text } }]
    });

    sse_frame(&value.to_string()).into_bytes()
}

fn finish_chunk(reason: &str) -> Vec<u8> {
    let value = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }]
    });

    sse_frame(&value.to_string()).into_bytes()
}

fn done_chunk() -> Vec<u8> {
    sse_frame("[DONE]").into_bytes()
}

fn tool_call_chunk(id: &str, name: &str, arguments: &str) -> Vec<u8> {
    let value = json!({
        "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0,
            "id": id,
            "function": { "name": name, "arguments": arguments }
        }] } }]
    });

    sse_frame(&value.to_string()).into_bytes()
}

fn usage_chunk(usage: Value) -> Vec<u8> {
    sse_frame(&json!({ "choices": [], "usage": usage }).to_string()).into_bytes()
}

async fn collect(
    client: &OpenCodeGoClient,
    request: ProviderRequest,
) -> Vec<Result<ProviderEvent, ProviderError>> {
    client.stream(request).collect().await
}

async fn batch_events(chunks: Vec<Vec<u8>>) -> Vec<Result<ProviderEvent, ProviderError>> {
    let server = FakeServer::start(vec![Reply::sse(chunks)]).await;
    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");

    collect(&client, request("go")).await
}

#[tokio::test]
async fn streams_text_from_the_go_chat_endpoint() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("Hel"),
        text_chunk("lo"),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let events = collect(&client, request("hi")).await;
    let text = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::TextDelta(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();

    assert_eq!(text, "Hello");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Finished(FinishReason::Stop))))
    );

    let recorded = server.requests();

    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].path, "/openai/v1/chat/completions");
    assert_eq!(
        recorded[0].authorization.as_deref(),
        Some("Bearer oc_sk_test")
    );
    assert_eq!(recorded[0].body["stream"], json!(true));
    assert_eq!(recorded[0].body["model"], json!("kimi-k3"));
    // Go asks for usage through `stream_options`, not OpenRouter's `usage`.
    assert_eq!(
        recorded[0].body["stream_options"],
        json!({ "include_usage": true })
    );
    assert!(recorded[0].body.get("usage").is_none());
}

#[tokio::test]
async fn the_session_header_reaches_the_wire_and_stays_stable() {
    let server = FakeServer::start(vec![
        Reply::sse(vec![finish_chunk("stop"), done_chunk()]),
        Reply::sse(vec![finish_chunk("stop"), done_chunk()]),
    ])
    .await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");

    let _ = collect(&client, request_for("one", "abc")).await;
    let _ = collect(&client, request_for("two", "abc")).await;

    let recorded = server.requests();

    assert_eq!(recorded.len(), 2);

    for record in &recorded {
        assert_eq!(record.header("x-opencode-session"), Some("ses_abc"));
        assert!(
            record
                .header("user-agent")
                .is_some_and(|agent| agent.starts_with("t8ful/")),
            "{record:?}"
        );
    }
}

#[tokio::test]
async fn two_conversations_get_different_sessions() {
    let server = FakeServer::start(vec![
        Reply::sse(vec![finish_chunk("stop"), done_chunk()]),
        Reply::sse(vec![finish_chunk("stop"), done_chunk()]),
    ])
    .await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");

    let _ = collect(&client, request_for("one", "first")).await;
    let _ = collect(&client, request_for("two", "second")).await;

    let recorded = server.requests();

    assert_eq!(recorded[0].header("x-opencode-session"), Some("ses_first"));
    assert_eq!(recorded[1].header("x-opencode-session"), Some("ses_second"));
}

#[tokio::test]
async fn reasoning_content_arrives_as_reasoning_and_never_as_text() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        reasoning_chunk("thinking"),
        text_chunk("answer"),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let events = collect(&client, request("hi")).await;
    let reasoning_index = events
        .iter()
        .position(
            |event| matches!(event, Ok(ProviderEvent::ReasoningDelta(text)) if text == "thinking"),
        )
        .expect("reasoning delta");
    let text_index = events
        .iter()
        .position(|event| matches!(event, Ok(ProviderEvent::TextDelta(text)) if text == "answer"))
        .expect("text delta");

    assert!(reasoning_index < text_index);
}

#[tokio::test]
async fn a_reasoning_effort_is_sent_flat() {
    let server =
        FakeServer::start(vec![Reply::sse(vec![finish_chunk("stop"), done_chunk()])]).await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let mut prompt = request("hi");

    prompt.reasoning = ReasoningEffort::High;

    let _ = collect(&client, prompt).await;

    let body: Value = server.requests()[0].body.clone();

    assert_eq!(body["reasoning_effort"], json!("high"));
    assert!(body.get("reasoning").is_none());
}

#[tokio::test]
async fn reconstructs_tool_calls_split_across_frames() {
    let fragments = [
        json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [
                { "index": 0, "id": "call_", "function": { "name": "read_", "arguments": "{\"pa" } }
            ] } }]
        }),
        json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [
                { "index": 0, "id": "abc", "function": { "name": "file", "arguments": "th\": \"src/main.rs\"}" } }
            ] } }]
        }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
    ];

    let payload = fragments
        .iter()
        .map(|fragment| sse_frame(&fragment.to_string()))
        .chain(std::iter::once(sse_frame("[DONE]")))
        .collect::<String>()
        .into_bytes();
    let server = FakeServer::start(vec![Reply::sse(chunked(payload, 3))]).await;
    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let events = collect(&client, request("read a file")).await;
    let calls = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::ToolCallComplete(call)) => Some(call.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id.as_str(), "call_abc");
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments, json!({ "path": "src/main.rs" }));
}

#[tokio::test]
async fn the_cost_and_opencode_billing_block_are_recorded() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("ok"),
        usage_chunk(json!({
            "prompt_tokens": 120,
            "completion_tokens": 30,
            "cost": 0.00042,
            "opencode": {
                "billing_source": "subscription",
                "tokens": {
                    "input": 118,
                    "output": 28,
                    "reasoning": 12,
                    "cache_read": 64,
                    "cache_write_5m": 16
                }
            }
        })),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let events = collect(&client, request("hi")).await;
    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(ProviderEvent::Usage(usage)) => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage.input_tokens, Some(120));
    assert_eq!(usage.output_tokens, Some(30));
    assert_eq!(usage.reasoning_tokens, Some(12));
    assert_eq!(usage.cache_read_tokens, Some(64));
    assert_eq!(usage.cache_write_tokens, Some(16));
    assert_eq!(usage.cost_usd.as_deref(), Some("0.00042"));
}

#[tokio::test]
async fn the_opencode_block_fills_in_missing_openai_fields() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("ok"),
        usage_chunk(json!({
            "cost": 0.0001,
            "opencode": {
                "tokens": { "input": 9, "output": 4, "cache_write_1h": 3 }
            }
        })),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let events = collect(&client, request("hi")).await;
    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(ProviderEvent::Usage(usage)) => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage.input_tokens, Some(9));
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.cache_write_tokens, Some(3));
    assert_eq!(usage.cache_read_tokens, None);
    assert_eq!(usage.reasoning_tokens, None);
}

#[tokio::test]
async fn an_in_band_error_frame_is_classified() {
    let frame = json!({
        "error": { "code": 429, "message": "slow down" },
        "choices": []
    });
    let events = batch_events(vec![sse_frame(&frame.to_string()).into_bytes()]).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::RateLimited { .. }))),
        "{events:?}"
    );
}

#[tokio::test]
async fn status_failures_map_to_actionable_errors() {
    type Case = (u16, fn(&ProviderError) -> bool, &'static str);

    let cases: Vec<Case> = vec![
        (401, |error| matches!(error, ProviderError::Auth(_)), "auth"),
        (403, |error| matches!(error, ProviderError::Auth(_)), "auth"),
        (
            404,
            |error| matches!(error, ProviderError::UnsupportedModel(_)),
            "unsupported model",
        ),
        (
            429,
            |error| matches!(error, ProviderError::RateLimited { .. }),
            "rate limit",
        ),
        (
            500,
            |error| matches!(error, ProviderError::Upstream { status: 500, .. }),
            "upstream",
        ),
    ];

    for (status, expected, label) in cases {
        let server = FakeServer::start(vec![Reply::status(
            status,
            json!({ "error": { "message": "provider said no" } }).to_string(),
        )])
        .await;
        let client =
            OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
        let events = collect(&client, request("hi")).await;

        assert!(
            events.iter().any(|event| match event {
                Err(error) => expected(error) && error.to_string().contains("provider said no"),
                Ok(_) => false,
            }),
            "a {status} response should classify as {label}: {events:?}"
        );
    }
}

#[tokio::test]
async fn the_catalog_is_read_from_the_go_models_endpoint() {
    let body = json!({
        "data": [{
            "id": "kimi-k3",
            "canonical_id": "moonshotai/kimi-k3",
            "limit": { "context": 262144, "output": 32768 },
            "capabilities": { "reasoning": { "effort": ["high", "medium", "low"] } },
            "modalities": { "input": ["text/plain"] },
            "cost": { "text": { "input": { "price": "0.0000006" }, "output": { "price": "0.0000025" } } }
        }]
    })
    .to_string();
    let server = FakeServer::start(vec![Reply::json(body)]).await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let models = client.list_models().await.expect("catalog");

    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "kimi-k3");
    assert_eq!(models[0].context_length, Some(262_144));
    assert_eq!(models[0].supports_images, Some(false));
    assert_eq!(models[0].prompt_price.as_deref(), Some("0.0000006"));

    let recorded = server.requests();

    assert_eq!(recorded[0].path, "/v1/models");
    assert_eq!(
        recorded[0].authorization.as_deref(),
        Some("Bearer oc_sk_test")
    );
}

#[tokio::test]
async fn the_catalog_works_without_a_key() {
    let body = json!({ "data": [{ "id": "kimi-k3" }] }).to_string();
    let server = FakeServer::start(vec![Reply::json(body)]).await;

    let client = OpenCodeGoClient::unauthenticated(server.base_url()).expect("client");
    let models = client.list_models().await.expect("catalog");

    assert_eq!(models.len(), 1);
    assert_eq!(server.requests()[0].authorization, None);
}

#[tokio::test]
async fn an_oversized_catalogue_is_rejected_before_parsing() {
    let oversized = "x".repeat(4 * 1024 * 1024 + 1);
    let body = format!("{{\"data\":[{{\"id\":\"{oversized}\"}}]}}");
    let server = FakeServer::start(vec![Reply::json(body)]).await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let error = client.list_models().await.expect_err("oversized catalogue");

    assert!(
        matches!(error, ProviderError::MalformedStream(_)),
        "{error}"
    );
    assert!(error.to_string().contains("larger than"), "{error}");
}

#[tokio::test]
async fn a_stream_that_dies_mid_tool_call_is_not_retried() {
    let server = FakeServer::start(vec![Reply::sse(vec![tool_call_chunk(
        "call-1",
        "read_file",
        "{\"path\":\"a.txt\"}",
    )])])
    .await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let events = collect(&client, request("go")).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::IncompleteStream(_)))),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::ToolCallComplete(_)))),
        "an incomplete batch must not emit calls: {events:?}"
    );
    // Exactly one request: a failed tool call is never replayed.
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_stalled_stream_fails_with_a_transport_error() {
    use std::time::Instant;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stall server");
    let addr = listener.local_addr().expect("stall server address");

    tokio::spawn(async move {
        let mut held = Vec::new();

        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let client = OpenCodeGoClient::with_base_url(format!("http://{addr}"), "oc_sk_test")
        .expect("client")
        .with_stream_idle_timeout(Duration::from_millis(150));

    let started = Instant::now();
    let events = collect(&client, request("hello")).await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the stalled stream took {:?} to fail",
        started.elapsed()
    );
    assert!(
        matches!(events.last(), Some(Err(ProviderError::Transport(message))) if message.contains("sent nothing")),
        "{events:?}"
    );
}

#[tokio::test]
async fn an_unauthenticated_stream_reports_missing_credentials() {
    let client = OpenCodeGoClient::unauthenticated("http://127.0.0.1:9").expect("client");
    let events = collect(&client, request("hi")).await;

    assert!(
        matches!(events.last(), Some(Err(ProviderError::MissingCredentials))),
        "{events:?}"
    );
    assert!(!format!("{client:?}").contains("oc_sk"));
}

#[tokio::test]
async fn an_empty_system_prompt_is_not_sent() {
    let server =
        FakeServer::start(vec![Reply::sse(vec![finish_chunk("stop"), done_chunk()])]).await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let mut prompt = request("hi");

    prompt.system.clear();

    let _ = collect(&client, prompt).await;

    let body = server.requests()[0].body.clone();

    assert_eq!(body["messages"][0]["role"], json!("user"));
}
