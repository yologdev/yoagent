//! Request-target tests for `AzureOpenAiProvider`.
//!
//! The mock only answers the exact documented Azure v1 request
//! (`POST /openai/v1/responses`, no `api-version`), so a wrong path or query
//! gets wiremock's 404 and the stream fails. The provider once sent
//! `{base}/responses?api-version=2025-01-01-preview`, which Azure never
//! served, and passed every test because the mocks matched any path.

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use yoagent::provider::{
    ApiProtocol, AzureOpenAiProvider, ModelConfig, StreamConfig, StreamProvider,
};
use yoagent::types::*;

fn responses_sse() -> String {
    [
        json!({"type": "response.created", "response": {"id": "r", "status": "in_progress", "output": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "ok"}),
        json!({"type": "response.completed", "response": {"id": "r", "status": "completed",
               "usage": {"input_tokens": 3, "output_tokens": 1}}}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

/// Mount a mock that answers only the documented v1 Responses request.
async fn v1_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/responses"))
        .and(query_param_is_missing("api-version"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(responses_sse(), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    server
}

/// Stream one request against `base_url`; return the request the server saw.
async fn send(
    server: &MockServer,
    base_url: String,
    api_key: &str,
    headers: &[(&str, &str)],
) -> Request {
    let mut mc = ModelConfig::custom(
        ApiProtocol::AzureOpenAiResponses,
        "azure",
        base_url,
        "gpt-5.5",
        "GPT-5.5",
    );
    for (k, v) in headers {
        mc.headers.insert((*k).into(), (*v).into());
    }
    let mut config = StreamConfig::new("gpt-5.5", api_key);
    config.messages = vec![Message::user("hi")];
    config.model_config = Some(mc);
    let (tx, _rx) = mpsc::unbounded_channel();
    AzureOpenAiProvider
        .stream(config, tx, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("stream failed: {e}"));
    let mut requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    requests.remove(0)
}

fn body(req: &Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

#[tokio::test]
async fn resource_root_base_url_posts_to_v1_responses_with_api_key() {
    let server = v1_server().await;
    let req = send(&server, server.uri(), "azure-key", &[]).await;
    assert_eq!(req.url.path(), "/openai/v1/responses");
    assert_eq!(req.url.query(), None, "v1 GA takes no query string");
    assert_eq!(req.headers.get("api-key").unwrap(), "azure-key");
    assert!(req.headers.get("authorization").is_none());
    // Without a deployment in the URL, `model` is the configured id, which
    // must be the deployment name.
    assert_eq!(body(&req)["model"], "gpt-5.5");
}

#[tokio::test]
async fn openai_v1_base_url_with_trailing_slash_is_not_doubled() {
    // The base URL Azure's own SDK samples use.
    let server = v1_server().await;
    let req = send(&server, format!("{}/openai/v1/", server.uri()), "k", &[]).await;
    assert_eq!(req.url.path(), "/openai/v1/responses");
    assert_eq!(body(&req)["model"], "gpt-5.5");
}

#[tokio::test]
async fn legacy_deployment_base_url_still_works_and_names_the_deployment() {
    // The form the docs used to recommend: routed to v1, deployment in `model`.
    let server = v1_server().await;
    let base = format!("{}/openai/deployments/prod-gpt", server.uri());
    let req = send(&server, base, "k", &[]).await;
    assert_eq!(req.url.path(), "/openai/v1/responses");
    assert_eq!(req.url.query(), None);
    assert_eq!(body(&req)["model"], "prod-gpt");
}

#[tokio::test]
async fn entra_id_bearer_without_a_key_sends_no_api_key_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/responses"))
        .and(header("authorization", "Bearer entra-token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(responses_sse(), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let req = send(
        &server,
        server.uri(),
        "",
        &[("Authorization", "Bearer entra-token")],
    )
    .await;
    assert!(
        req.headers.get("api-key").is_none(),
        "an empty api-key header would be sent alongside the token"
    );
}
