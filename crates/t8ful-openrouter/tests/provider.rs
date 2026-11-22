use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use t8ful_core::error::ProviderError;
use t8ful_core::message::Conversation;
use t8ful_core::provider::{
    FinishReason, Provider, ProviderEvent, ProviderRequest, ReasoningEffort,
};
use t8ful_openai::test_support::{FakeServer, Reply, chunked, sse_frame};
use t8ful_openrouter::OpenRouterClient;

fn request(prompt: &str) -> ProviderRequest {
    let mut conversation = Conversation::new();

    conversation.push_user_text(prompt);

    ProviderRequest {
        model: "openrouter/test-model".to_owned(),
        system: "be helpful".to_owned(),
        messages: conversation.messages().to_vec(),
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: ReasoningEffort::Default,
        reasoning_value: None,
        session_id: Some("ses_test".to_owned()),
    }
}

fn text_chunk(text: &str) -> Vec<u8> {
    let value = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "content": text } }]
    });

    sse_frame(&value.to_string()).into_bytes()
}

fn reasoning_chunk(text: &str) -> Vec<u8> {
    let value = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "reasoning": text } }]
    });

    sse_frame(&value.to_string()).into_bytes()
}

fn reasoning_content_chunk(text: &str) -> Vec<u8> {
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

async fn collect(
    client: &OpenRouterClient,
    request: ProviderRequest,
) -> Vec<Result<ProviderEvent, ProviderError>> {
    client.stream(request).collect().await
}

#[tokio::test]
async fn streams_text_and_reports_completion() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("Hel"),
        text_chunk("lo"),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
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
    assert_eq!(recorded[0].path, "/chat/completions");
    assert_eq!(
        recorded[0].authorization.as_deref(),
        Some("Bearer test-key")
    );
    assert_eq!(recorded[0].body["stream"], json!(true));
    assert_eq!(recorded[0].body["usage"]["include"], json!(true));
}

#[tokio::test]
async fn reasoning_precedes_content_and_is_never_text() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        reasoning_chunk("thinking"),
        text_chunk("answer"),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
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

    let text = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::TextDelta(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();

    assert_eq!(text, "answer");
}

#[tokio::test]
async fn both_reasoning_field_spellings_are_accepted() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        reasoning_chunk("first"),
        reasoning_content_chunk("second"),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;
    let reasoning = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::ReasoningDelta(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();

    assert_eq!(reasoning, "firstsecond");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::TextDelta(_))))
    );
}

#[tokio::test]
async fn reasoning_survives_arbitrary_sse_split_points() {
    let payload = [
        reasoning_chunk("why "),
        reasoning_chunk("and how"),
        text_chunk(" answer"),
        finish_chunk("stop"),
        done_chunk(),
    ]
    .concat();
    let server = FakeServer::start(vec![Reply::sse(chunked(payload, 3))]).await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;
    let reasoning = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::ReasoningDelta(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();

    assert_eq!(reasoning, "why and how");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::TextDelta(text)) if text == " answer"))
    );
}

#[tokio::test]
async fn a_reasoning_only_frame_does_not_produce_text() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        reasoning_chunk("only"),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;

    assert!(
        events.iter().any(
            |event| matches!(event, Ok(ProviderEvent::ReasoningDelta(text)) if text == "only")
        )
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::TextDelta(_))))
    );
}

#[tokio::test]
async fn reconstructs_tool_calls_split_at_arbitrary_offsets() {
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
        json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
        }),
    ];

    let payload = fragments
        .iter()
        .map(|fragment| sse_frame(&fragment.to_string()))
        .chain(std::iter::once(sse_frame("[DONE]")))
        .collect::<String>()
        .into_bytes();

    // Three-byte chunks cross frame, line, and JSON boundaries repeatedly.
    let chunks = payload.chunks(3).map(<[u8]>::to_vec).collect::<Vec<_>>();
    let server = FakeServer::start(vec![Reply::sse(chunks)]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
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
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Finished(FinishReason::ToolCalls)))),
        "events: {events:?}"
    );
}

#[tokio::test]
async fn rejects_tool_calls_with_invalid_arguments() {
    let fragment = json!({
        "choices": [{ "index": 0, "delta": { "tool_calls": [
            { "index": 0, "id": "call-1", "function": { "name": "read_file", "arguments": "{not json" } }
        ] } }]
    });

    let server = FakeServer::start(vec![Reply::sse(vec![
        sse_frame(&fragment.to_string()).into_bytes(),
        finish_chunk("tool_calls"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("go")).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            Err(ProviderError::MalformedToolCall { index, .. }) if *index == 0
        )),
        "events: {events:?}"
    );
}

#[tokio::test]
async fn incomplete_stream_is_reported() {
    let server = FakeServer::start(vec![Reply::sse(vec![text_chunk("partial")])]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::IncompleteStream(_)))),
        "events: {events:?}"
    );
}

#[tokio::test]
async fn missing_usage_is_not_reported_as_zero() {
    let usage = json!({
        "choices": [],
        "usage": { "prompt_tokens": 11, "completion_tokens": null }
    });

    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("ok"),
        sse_frame(&usage.to_string()).into_bytes(),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;

    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(ProviderEvent::Usage(usage)) => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, None);
}

#[tokio::test]
async fn cached_prompt_tokens_are_recorded_for_context_accounting() {
    let usage = json!({
        "choices": [],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": { "cached_tokens": 60 }
        }
    });

    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("ok"),
        sse_frame(&usage.to_string()).into_bytes(),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;
    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(ProviderEvent::Usage(usage)) => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage.cache_read_tokens, Some(60));
    // The prompt count already includes the cached tokens.
    assert_eq!(usage.context_tokens(), Some(110));
}

#[tokio::test]
async fn reasoning_tokens_are_parsed_but_stay_outside_the_context_count() {
    let usage = json!({
        "choices": [],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 40,
            "completion_tokens_details": { "reasoning_tokens": 25 }
        }
    });

    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("ok"),
        sse_frame(&usage.to_string()).into_bytes(),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;
    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(ProviderEvent::Usage(usage)) => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage.reasoning_tokens, Some(25));
    assert_eq!(usage.context_tokens(), Some(140));
}

#[tokio::test]
async fn absent_reasoning_tokens_stay_unknown() {
    let usage = json!({
        "choices": [],
        "usage": { "prompt_tokens": 5, "completion_tokens": 6 }
    });

    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("ok"),
        sse_frame(&usage.to_string()).into_bytes(),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;
    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(ProviderEvent::Usage(usage)) => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage.reasoning_tokens, None);
}

#[tokio::test]
async fn provider_reported_cost_is_kept_as_a_decimal_string() {
    let usage = json!({
        "choices": [],
        "usage": { "prompt_tokens": 5, "completion_tokens": 6, "cost": 0.000123 }
    });

    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("ok"),
        sse_frame(&usage.to_string()).into_bytes(),
        finish_chunk("stop"),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let events = collect(&client, request("hi")).await;

    let usage = events
        .iter()
        .find_map(|event| match event {
            Ok(ProviderEvent::Usage(usage)) => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage.cost_usd.as_deref(), Some("0.000123"));
}

#[tokio::test]
async fn authentication_failure_is_actionable() {
    let server = FakeServer::start(vec![Reply::status(401, "{\"error\":\"bad key\"}")]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "nope").expect("client");
    let events = collect(&client, request("hi")).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::Auth(message)) if message.contains("bad key"))),
        "events: {events:?}"
    );
}

#[tokio::test]
async fn rate_limit_reports_retry_after() {
    let server = FakeServer::start(vec![Reply::status(429, "{\"error\":\"slow down\"}")]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("hi")).await;

    let retry_after = events.iter().find_map(|event| match event {
        Err(ProviderError::RateLimited { retry_after_ms, .. }) => Some(*retry_after_ms),
        _ => None,
    });

    // The fake server sends no Retry-After header, so nothing says when to
    // come back and the request is not re-sent.
    assert_eq!(retry_after, Some(None));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_rate_limited_request_is_retried_after_its_retry_after() {
    let server = FakeServer::start(vec![
        Reply::status(429, "{\"error\":\"slow down\"}").with_header("retry-after", "0"),
        Reply::sse(vec![
            text_chunk("Hello"),
            finish_chunk("stop"),
            done_chunk(),
        ]),
    ])
    .await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("hi")).await;

    let warnings = events
        .iter()
        .filter(|event| matches!(event, Ok(ProviderEvent::Warning(_))))
        .count();

    assert_eq!(warnings, 1, "one warning per retry: {events:?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::TextDelta(text)) if text == "Hello")),
        "the retry must deliver the response: {events:?}"
    );

    let recorded = server.requests();

    assert_eq!(recorded.len(), 2, "one rejection and one retry");
    assert_eq!(
        recorded[0].body, recorded[1].body,
        "the retry resends the same body"
    );
    assert_eq!(recorded[1].body["stream"], json!(true));
}

#[tokio::test]
async fn an_over_cap_retry_after_is_reported_instead_of_waited_out() {
    let server = FakeServer::start(vec![
        Reply::status(429, "{\"error\":\"slow down\"}").with_header("retry-after", "300"),
    ])
    .await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("hi")).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::RateLimited { .. }))),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Warning(_)))),
        "an unaffordable wait must not announce a retry: {events:?}"
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_mid_stream_rate_limit_is_never_retried() {
    // The deltas already reached the consumer, so re-sending the request would
    // duplicate them. The frame lands after a text delta for exactly that case.
    let frame = json!({
        "error": { "code": 429, "message": "slow down" },
        "choices": []
    });
    let server = FakeServer::start(vec![Reply::sse(vec![
        text_chunk("He"),
        sse_frame(&frame.to_string()).into_bytes(),
    ])])
    .await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("hi")).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::RateLimited { .. }))),
        "{events:?}"
    );
    assert_eq!(server.requests().len(), 1, "a stream error is not a retry");
}

#[tokio::test]
async fn an_in_flight_budget_refusal_is_retried_but_exhausted_credits_are_not() {
    let refusal = |source: &str| {
        Reply::status(
            402,
            json!({ "error": { "message": "payment required", "metadata": { "limit_source": source } } })
                .to_string(),
        )
        .with_header("retry-after", "0")
    };

    // A concurrency budget frees up on its own, so waiting can succeed.
    let server = FakeServer::start(vec![
        refusal("openrouter_in_flight_budget"),
        Reply::sse(vec![
            text_chunk("Hello"),
            finish_chunk("stop"),
            done_chunk(),
        ]),
    ])
    .await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("hi")).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::TextDelta(text)) if text == "Hello")),
        "{events:?}"
    );
    assert_eq!(server.requests().len(), 2);

    // An exhausted balance never clears by waiting, so it is reported at once
    // with the fix the user has to make.
    let server = FakeServer::start(vec![refusal("openrouter_credits")]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("hi")).await;
    let message = events
        .iter()
        .find_map(|event| match event {
            Err(error @ ProviderError::PaymentRequired { .. }) => Some(error.to_string()),
            _ => None,
        })
        .expect("a payment refusal");

    assert!(message.contains("add credits"), "{message}");
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn dropping_the_stream_during_a_retry_wait_stops_the_pump() {
    let server = FakeServer::start(vec![
        Reply::status(429, "{\"error\":\"slow down\"}").with_header("retry-after", "120"),
        Reply::sse(vec![text_chunk("late"), finish_chunk("stop"), done_chunk()]),
    ])
    .await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let mut stream = client.stream(request("hi"));

    // The first item is the retry notice; the pump is then sleeping out the
    // 120-second wait that the consumer no longer wants.
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the retry notice arrives without waiting")
        .expect("an event");

    assert!(
        matches!(&first, Ok(ProviderEvent::Warning(message)) if message.contains("retry 2/5")),
        "{first:?}"
    );

    drop(stream);
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        server.requests().len(),
        1,
        "an abandoned turn must not re-send the request"
    );
}

#[tokio::test]
async fn upstream_failures_are_bounded() {
    let body = format!("<html>{}</html>", "x".repeat(10_000));
    let server = FakeServer::start(vec![Reply::status(500, body)]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("hi")).await;

    let message = events
        .iter()
        .find_map(|event| match event {
            Err(ProviderError::Upstream { status, message }) => Some((*status, message.clone())),
            _ => None,
        })
        .expect("upstream error");

    assert_eq!(message.0, 500);
    assert!(message.1.chars().count() <= 513);
}

#[tokio::test]
async fn a_missing_attachment_is_reported_without_panicking() {
    let server = FakeServer::start(Vec::new()).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "test-key").expect("client");
    let mut request = request("describe this image");

    request.messages.push(t8ful_core::Message::user_with_images(
        Some("attachment".to_owned()),
        vec![t8ful_core::content::ImageInput::new(
            "image/png",
            std::path::PathBuf::from("/definitely/not/a/real-t8ful-image.png"),
            1,
            1,
            8,
        )],
    ));

    let events = collect(&client, request).await;
    let error = events
        .iter()
        .find_map(|event| match event {
            Err(error) => Some(error.to_string()),
            Ok(_) => None,
        })
        .expect("provider error");

    assert!(error.contains("missing or unreadable"), "{error}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn missing_credentials_are_distinct_from_network_failures() {
    let error = match OpenRouterClient::from_api_key(None, "http://[IP_ADDRESS]:1") {
        Ok(_) => panic!("expected a missing credential error"),
        Err(error) => error,
    };

    assert!(matches!(error, ProviderError::MissingCredentials));

    let empty = OpenRouterClient::from_api_key(Some(String::new()), "http://[IP_ADDRESS]:1")
        .expect_err("empty key is rejected");

    assert!(matches!(empty, ProviderError::MissingCredentials));
}

#[tokio::test]
async fn model_discovery_reads_the_advertised_reasoning_efforts() {
    // Shapes taken from the live OpenRouter catalog: a model that lists its
    // efforts, a mandatory one that omits `none`, and one with the flag only.
    let body = json!({
        "data": [
            { "id": "vendor/sol", "supported_parameters": ["reasoning"],
              "reasoning": { "mandatory": true, "default_enabled": true,
                             "supported_efforts": ["max", "xhigh", "high", "medium", "low"],
                             "default_effort": "medium" } },
            { "id": "vendor/flexible", "supported_parameters": ["reasoning"],
              "reasoning": { "mandatory": false, "default_enabled": true,
                             "supported_efforts": ["high", "low", "none"] } },
            { "id": "vendor/flag-only", "supported_parameters": ["reasoning"] }
        ]
    });

    let server = FakeServer::start(vec![Reply::json(body.to_string())]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let models = client.list_models().await.expect("model list");
    let levels = |index: usize| models[index].reasoning.as_ref().map(|p| p.levels());

    assert_eq!(
        levels(0),
        Some(vec![
            ReasoningEffort::Default,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ])
    );
    assert_eq!(
        levels(1),
        Some(vec![
            ReasoningEffort::Default,
            ReasoningEffort::Off,
            ReasoningEffort::Low,
            ReasoningEffort::High,
        ])
    );
    // Without a list, the flag still decides between default/off and through high.
    assert_eq!(levels(2), None);
    assert_eq!(models[2].supports_reasoning, Some(true));
}

#[tokio::test]
async fn model_discovery_tolerates_missing_metadata() {
    let body = json!({
        "data": [
            { "id": "vendor/model-a", "name": "Model A", "context_length": 128000,
              "supported_parameters": ["tools", "reasoning"],
              "pricing": { "prompt": "0.000001", "completion": 0.000002 } },
            { "id": "vendor/model-b" },
            { "id": "vendor/model-c", "supported_parameters": ["tools"] }
        ]
    });

    let server = FakeServer::start(vec![Reply::json(body.to_string())]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let models = client.list_models().await.expect("model list");

    assert_eq!(models.len(), 3);
    assert_eq!(models[0].id, "vendor/model-a");
    assert_eq!(models[0].context_length, Some(128_000));
    assert_eq!(models[0].supports_reasoning, Some(true));
    assert_eq!(models[0].prompt_price.as_deref(), Some("0.000001"));
    assert_eq!(models[1].id, "vendor/model-b");
    assert!(models[1].context_length.is_none());
    assert_eq!(models[1].supports_reasoning, None);
    assert_eq!(models[2].supports_reasoning, Some(false));

    let recorded = server.requests();

    assert_eq!(recorded[0].path, "/models");
}

#[tokio::test]
async fn an_oversized_model_catalogue_is_rejected_before_parsing() {
    let oversized = "x".repeat(4 * 1024 * 1024 + 1);
    let body = format!("{{\"data\":[{{\"id\":\"{oversized}\"}}]}}");
    let server = FakeServer::start(vec![Reply::json(body)]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let error = client.list_models().await.expect_err("oversized catalogue");

    assert!(
        matches!(error, ProviderError::MalformedStream(_)),
        "{error}"
    );
    assert!(error.to_string().contains("larger than"), "{error}");
}

#[tokio::test]
async fn error_bodies_are_read_only_to_the_cap() {
    let body = format!("{}{}", " ".repeat(64 * 1024), "AFTER-CAP");
    let server = FakeServer::start(vec![Reply::status(500, body)]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("hi")).await;

    let message = events
        .iter()
        .find_map(|event| match event {
            Err(ProviderError::Upstream { status, message }) => Some((*status, message.clone())),
            _ => None,
        })
        .expect("upstream error");

    assert_eq!(message.0, 500);
    assert!(
        !message.1.contains("AFTER-CAP"),
        "the error body was read past the cap: {}",
        message.1
    );
    assert!(message.1.contains("no error detail"), "{}", message.1);
}

#[tokio::test]
async fn dropping_a_stream_midway_does_not_report_a_fake_completion() {
    let server = FakeServer::start(vec![
        Reply::sse(vec![
            text_chunk("one"),
            text_chunk("two"),
            finish_chunk("stop"),
            done_chunk(),
        ])
        .with_delay(Duration::from_millis(20)),
    ])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let mut stream = client.stream(request("hi"));

    let first = stream
        .next()
        .await
        .expect("first event")
        .expect("text event");

    assert!(matches!(first, ProviderEvent::TextDelta(_)));

    drop(stream);

    // The fake server keeps serving; a later request still works and no
    // completion event was synthesized for the abandoned stream.
    tokio::time::sleep(Duration::from_millis(60)).await;
}

#[tokio::test]
async fn the_system_prompt_is_sent_as_the_first_message() {
    let server =
        FakeServer::start(vec![Reply::sse(vec![finish_chunk("stop"), done_chunk()])]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let _ = collect(&client, request("hi")).await;

    let body = server.requests()[0].body.clone();

    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][0]["content"], json!("be helpful"));
    assert_eq!(body["messages"][1]["role"], json!("user"));
    assert_eq!(body["messages"][1]["content"], json!("hi"));
}

#[tokio::test]
async fn an_empty_system_prompt_is_not_sent() {
    let server =
        FakeServer::start(vec![Reply::sse(vec![finish_chunk("stop"), done_chunk()])]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let mut prompt = request("hi");

    prompt.system.clear();

    let _ = collect(&client, prompt).await;

    let body = server.requests()[0].body.clone();

    assert_eq!(body["messages"][0]["role"], json!("user"));
}

#[tokio::test]
async fn dropping_a_stalled_stream_closes_the_connection() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind stalled server");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];

        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.expect("read request");

            assert!(read > 0, "client closed before sending a request");
            request.extend_from_slice(&chunk[..read]);
        }

        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                  Cache-Control: no-cache\r\n\r\n",
            )
            .await
            .expect("write headers");
        socket
            .write_all(&text_chunk("first"))
            .await
            .expect("write chunk");
        socket.flush().await.expect("flush");

        let closed = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut chunk)).await;

        assert!(
            matches!(closed, Ok(Ok(0))),
            "the abandoned HTTP connection stayed open: {closed:?}"
        );
    });

    let client =
        OpenRouterClient::with_base_url(format!("http://{address}"), "key").expect("client");
    let mut stream = client.stream(request("hi"));
    let first = stream.next().await.expect("first event").expect("text");

    assert!(matches!(first, ProviderEvent::TextDelta(_)));

    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server observed the close")
        .expect("server task");
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

async fn batch_events(chunks: Vec<Vec<u8>>) -> Vec<Result<ProviderEvent, ProviderError>> {
    let server = FakeServer::start(vec![Reply::sse(chunks)]).await;
    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");

    collect(&client, request("go")).await
}

#[tokio::test]
async fn tool_batch_valid_json_eof_without_finish_is_rejected() {
    let events = batch_events(vec![tool_call_chunk(
        "call-1",
        "read_file",
        "{\"path\":\"a.txt\"}",
    )])
    .await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::IncompleteStream(_)))),
        "events: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::ToolCallComplete(_)))),
        "an incomplete batch must not emit calls: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Finished(_)))),
        "an incomplete batch must not report completion: {events:?}"
    );
}

#[tokio::test]
async fn tool_batch_done_without_a_finish_reason_is_rejected() {
    let events = batch_events(vec![
        tool_call_chunk("call-1", "read_file", "{\"path\":\"a.txt\"}"),
        done_chunk(),
    ])
    .await;

    assert!(
        events.iter().any(|event| event.is_err()),
        "events: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::ToolCallComplete(_)))),
        "an unterminated batch must not emit calls: {events:?}"
    );
}

#[tokio::test]
async fn tool_batch_with_a_length_finish_is_rejected() {
    let events = batch_events(vec![
        tool_call_chunk("call-1", "read_file", "{\"path\":\"a.txt\"}"),
        finish_chunk("length"),
        done_chunk(),
    ])
    .await;

    assert!(
        events.iter().any(|event| event.is_err()),
        "events: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::ToolCallComplete(_)))),
        "a length-limited batch must not emit calls: {events:?}"
    );
}

#[tokio::test]
async fn tool_batch_with_a_stop_finish_is_rejected() {
    let events = batch_events(vec![
        tool_call_chunk("call-1", "read_file", "{\"path\":\"a.txt\"}"),
        finish_chunk("stop"),
        done_chunk(),
    ])
    .await;

    assert!(
        events.iter().any(|event| event.is_err()),
        "events: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::ToolCallComplete(_)))),
        "a stopped batch must not emit calls: {events:?}"
    );
}

#[tokio::test]
async fn tool_batch_with_a_tool_calls_finish_is_accepted() {
    let events = batch_events(vec![
        tool_call_chunk("call-1", "read_file", "{\"path\":\"a.txt\"}"),
        finish_chunk("tool_calls"),
        done_chunk(),
    ])
    .await;

    let calls = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::ToolCallComplete(call)) => Some(call.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id.as_str(), "call-1");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Finished(FinishReason::ToolCalls)))),
        "events: {events:?}"
    );
}

#[tokio::test]
async fn text_only_length_is_still_reported_as_length() {
    let events = batch_events(vec![
        text_chunk("partial"),
        finish_chunk("length"),
        done_chunk(),
    ])
    .await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Finished(FinishReason::Length)))),
        "events: {events:?}"
    );
}

#[tokio::test]
async fn parallel_tool_calls_keep_their_index_order() {
    let fragments = [
        json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [
                { "index": 0, "id": "call-a", "function": { "name": "read_file", "arguments": "{\"path\":\"a\"}" } },
                { "index": 1, "id": "call-b", "function": { "name": "read_file", "arguments": "{\"path\":\"b\"}" } }
            ] } }]
        }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
    ];

    let server = FakeServer::start(vec![Reply::sse(vec![
        sse_frame(&fragments[0].to_string()).into_bytes(),
        sse_frame(&fragments[1].to_string()).into_bytes(),
        done_chunk(),
    ])])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");
    let events = collect(&client, request("go")).await;

    let names = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::ToolCallComplete(call)) => Some(call.id.as_str().to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["call-a".to_owned(), "call-b".to_owned()]);

    let body: Value = server.requests()[0].body.clone();

    assert_eq!(body["messages"][1]["content"], json!("go"));
}

/// A provider that accepts the request and then sends nothing must fail instead
/// of holding the turn open forever.
#[tokio::test]
async fn a_stalled_stream_fails_with_a_transport_error() {
    use std::time::Instant;

    // Accept the connection and keep it open without writing a single byte: the
    // response headers never arrive.
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

    let client = OpenRouterClient::with_base_url(format!("http://{addr}"), "key")
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

/// OpenRouter attributes traffic by these headers. Losing them silently removes
/// t8ful from the rankings it is meant to appear in, so both requests assert
/// them explicitly.
#[tokio::test]
async fn every_request_carries_the_openrouter_attribution_headers() {
    let models = json!({ "data": [{ "id": "vendor/model" }] }).to_string();
    let server = FakeServer::start(vec![
        Reply::json(models),
        Reply::sse(vec![finish_chunk("stop"), done_chunk()]),
    ])
    .await;

    let client = OpenRouterClient::with_base_url(server.base_url(), "key").expect("client");

    client.list_models().await.expect("catalog");
    let _ = collect(&client, request("hi")).await;

    let recorded = server.requests();

    assert_eq!(recorded.len(), 2, "{recorded:?}");

    for record in &recorded {
        let user_agent = record.header("user-agent").unwrap_or_default();

        assert!(
            user_agent.starts_with("t8ful/"),
            "{} sent user-agent {user_agent}",
            record.path
        );
        assert_eq!(
            record.header("http-referer"),
            Some("https://github.com/deathlesz/t8ful"),
            "{}",
            record.path
        );
        assert_eq!(record.header("x-openrouter-title"), Some("t8ful"));
        assert_eq!(record.header("x-openrouter-categories"), Some("cli-agent"));
    }
}
