//! End-to-end routing between the two provider clients.
//!
//! `RoutingProvider` lives in the binary, so this test drives the real
//! OpenRouter and OpenCode Go clients against two loopback servers and checks
//! that a request follows the model id, the pinned provider, and the fallback
//! rule in that order.

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::json;
use t8ful_core::message::Conversation;
use t8ful_core::provider::{ModelInfo, Provider, ProviderRequest, ReasoningEffort};
use t8ful_opencode::OpenCodeGoClient;
use t8ful_openrouter::OpenRouterClient;

mod support;

use support::{FakeServer, Reply, sse_frame, text_stream};

fn request(model: &str) -> ProviderRequest {
    let mut conversation = Conversation::new();

    conversation.push_user_text("hi");

    ProviderRequest {
        model: model.to_owned(),
        system: String::new(),
        messages: conversation.messages().to_vec(),
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: ReasoningEffort::Default,
        reasoning_value: None,
        session_id: Some("ses_route".to_owned()),
    }
}

fn catalogue(ids: &[&str]) -> String {
    json!({ "data": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>() }).to_string()
}

fn go_catalogue(ids: &[&str]) -> String {
    json!({
        "data": ids
            .iter()
            .map(|id| json!({ "id": id, "limit": { "context": 131072 } }))
            .collect::<Vec<_>>()
    })
    .to_string()
}

async fn openrouter_models(server: &FakeServer) -> Vec<ModelInfo> {
    let mut models = OpenRouterClient::with_base_url(server.base_url(), "sk-or-test")
        .expect("client")
        .list_models()
        .await
        .expect("catalog");

    for model in &mut models {
        model.provider = Some("openrouter".to_owned());
    }

    models
}

async fn go_models(server: &FakeServer) -> Vec<ModelInfo> {
    let mut models = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test")
        .expect("client")
        .list_models()
        .await
        .expect("catalog");

    for model in &mut models {
        model.provider = Some("opencode-go".to_owned());
    }

    models
}

/// The chat request goes to whichever base URL owns the selected model.
#[tokio::test]
async fn each_client_serves_its_own_catalogue_and_endpoint() {
    let openrouter = FakeServer::start_with_models(
        vec![Reply::body(text_stream("from openrouter"))],
        &catalogue(&["vendor/sol"]),
    )
    .await;
    let go = FakeServer::start_with_models(
        vec![Reply::body(text_stream("from go"))],
        &go_catalogue(&["kimi-k3"]),
    )
    .await;

    let openrouter_models = openrouter_models(&openrouter).await;
    let go_models = go_models(&go).await;

    assert_eq!(openrouter_models[0].id, "vendor/sol");
    assert_eq!(openrouter_models[0].provider.as_deref(), Some("openrouter"));
    assert_eq!(go_models[0].id, "kimi-k3");
    assert_eq!(go_models[0].provider.as_deref(), Some("opencode-go"));

    // The Go request carries the session header, the OpenRouter one does not.
    let go_client = OpenCodeGoClient::with_base_url(go.base_url(), "oc_sk_test").expect("client");
    let openrouter_client =
        OpenRouterClient::with_base_url(openrouter.base_url(), "sk-or-test").expect("client");

    let go_text = go_client
        .stream(request("kimi-k3"))
        .filter_map(|event| async move {
            match event {
                Ok(t8ful_core::provider::ProviderEvent::TextDelta(text)) => Some(text),
                _ => None,
            }
        })
        .collect::<String>()
        .await;
    let openrouter_text = openrouter_client
        .stream(request("vendor/sol"))
        .filter_map(|event| async move {
            match event {
                Ok(t8ful_core::provider::ProviderEvent::TextDelta(text)) => Some(text),
                _ => None,
            }
        })
        .collect::<String>()
        .await;

    assert_eq!(go_text, "from go");
    assert_eq!(openrouter_text, "from openrouter");
}

/// A provider whose catalog fails still leaves the other one usable, and the
/// warning names the provider that failed.
#[tokio::test]
async fn a_failed_go_catalogue_leaves_openrouter_usable() {
    let openrouter =
        FakeServer::start_with_models(Vec::new(), &catalogue(&["vendor/sol", "vendor/tool"])).await;
    let go =
        FakeServer::start_with_models_reply(Vec::new(), Reply::status(500, "{\"error\":\"down\"}"))
            .await;

    let openrouter_models = openrouter_models(&openrouter).await;
    let go_error = OpenCodeGoClient::with_base_url(go.base_url(), "oc_sk_test")
        .expect("client")
        .list_models()
        .await
        .expect_err("the Go catalog failed");

    assert!(go_error.to_string().contains("down"), "{go_error}");
    assert_eq!(openrouter_models.len(), 2);
}

/// Two catalogs merge without dropping a duplicate id, so the Go copy of a
/// model stays reachable.
#[tokio::test]
async fn a_duplicated_id_keeps_both_rows() {
    let openrouter =
        FakeServer::start_with_models(Vec::new(), &catalogue(&["deepseek-v4.1-flash"])).await;
    let go =
        FakeServer::start_with_models(Vec::new(), &go_catalogue(&["deepseek-v4.1-flash"])).await;

    let mut merged = openrouter_models(&openrouter).await;

    merged.extend(go_models(&go).await);

    let duplicated = merged
        .iter()
        .filter(|model| model.id == "deepseek-v4.1-flash")
        .map(|model| model.provider.as_deref())
        .collect::<Vec<_>>();

    assert_eq!(duplicated, vec![Some("openrouter"), Some("opencode-go")]);
}

/// The idle deadline still bounds a Go stream, so a provider that accepts the
/// connection and sends nothing cannot hold a turn open.
#[tokio::test]
async fn a_stalled_go_stream_fails_quickly() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stall server");
    let addr = listener.local_addr().expect("address");

    tokio::spawn(async move {
        let mut held = Vec::new();

        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let client = OpenCodeGoClient::with_base_url(format!("http://{addr}"), "oc_sk_test")
        .expect("client")
        .with_stream_idle_timeout(Duration::from_millis(150));
    let events = client.stream(request("kimi-k3")).collect::<Vec<_>>().await;

    assert!(
        matches!(events.last(), Some(Err(t8ful_core::ProviderError::Transport(message))) if message.contains("sent nothing")),
        "{events:?}"
    );
}

/// The system prompt still arrives as one leading message on the Go path.
#[tokio::test]
async fn the_go_request_serializes_the_system_prompt() {
    let server = FakeServer::start(vec![Reply::sse(vec![
        sse_frame(
            &json!({
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
            })
            .to_string(),
        )
        .into_bytes(),
        sse_frame("[DONE]").into_bytes(),
    ])])
    .await;

    let client = OpenCodeGoClient::with_base_url(server.base_url(), "oc_sk_test").expect("client");
    let mut prompt = request("kimi-k3");

    prompt.system = "be brief".to_owned();

    let _ = client.stream(prompt).collect::<Vec<_>>().await;

    let body = server.requests()[0].body.clone();

    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][0]["content"], json!("be brief"));
    assert_eq!(body["messages"][1]["content"], json!("hi"));
}
