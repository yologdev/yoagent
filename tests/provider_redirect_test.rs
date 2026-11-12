//! A provider request answered with a redirect to another origin must send
//! nothing there (yoyo-evolve#1181). reqwest's default policy followed it,
//! stripping only `Authorization`/`Cookie`/`Proxy-Authorization`: the second
//! host received the whole prompt and, from Anthropic's `x-api-key` and
//! Azure's `api-key`, the key — on every retry.
//!
//! Two local servers: `first` answers every request with a 307/308 to
//! `elsewhere`, which must receive nothing. Each provider's request fails
//! with a non-retryable error naming the other origin. A redirect within the
//! same origin is still followed.

use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::{
    AnthropicProvider, ApiProtocol, AzureOpenAiProvider, BedrockProvider, GoogleProvider,
    GoogleVertexProvider, ModelConfig, OpenAiCompatProvider, OpenAiResponsesProvider,
    ProviderError, StreamConfig, StreamProvider,
};
use yoagent::types::*;

const CANARY: &str = "bedrock-api-key-canary0123456789";
const PROMPT: &str = "the secret prompt";

/// `first` redirects everything to `elsewhere` with `status`; `elsewhere`
/// answers 200 and must never be reached.
async fn redirecting_pair(status: u16) -> (MockServer, MockServer) {
    let first = MockServer::start().await;
    let elsewhere = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("location", format!("{}/stolen", elsewhere.uri())),
        )
        .mount(&first)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&elsewhere)
        .await;
    (first, elsewhere)
}

async fn send(
    provider: &dyn StreamProvider,
    api: ApiProtocol,
    base_url: String,
) -> Result<Message, ProviderError> {
    let mc = ModelConfig::custom(api, "test", base_url, "model-x", "Model X");
    let mut config = StreamConfig::new("model-x", CANARY);
    config.messages = vec![Message::user(PROMPT)];
    config.model_config = Some(mc);
    let (tx, _rx) = mpsc::unbounded_channel();
    provider.stream(config, tx, CancellationToken::new()).await
}

fn providers() -> Vec<(&'static str, Box<dyn StreamProvider>, ApiProtocol)> {
    vec![
        (
            "anthropic",
            Box::new(AnthropicProvider),
            ApiProtocol::AnthropicMessages,
        ),
        (
            "openai-compatible",
            Box::new(OpenAiCompatProvider),
            ApiProtocol::OpenAiCompletions,
        ),
        (
            "openai responses",
            Box::new(OpenAiResponsesProvider),
            ApiProtocol::OpenAiResponses,
        ),
        (
            "azure",
            Box::new(AzureOpenAiProvider),
            ApiProtocol::AzureOpenAiResponses,
        ),
        (
            "google",
            Box::new(GoogleProvider),
            ApiProtocol::GoogleGenerativeAi,
        ),
        (
            "vertex",
            Box::new(GoogleVertexProvider),
            ApiProtocol::GoogleVertex,
        ),
        (
            "bedrock",
            Box::new(BedrockProvider),
            ApiProtocol::BedrockConverseStream,
        ),
    ]
}

#[tokio::test]
async fn no_provider_follows_a_redirect_to_another_origin() {
    for status in [307, 308] {
        let (first, elsewhere) = redirecting_pair(status).await;
        for (name, provider, api) in providers() {
            let err = send(provider.as_ref(), api, first.uri())
                .await
                .expect_err(name);
            assert!(
                matches!(&err, ProviderError::Other(m) if m.contains(&elsewhere.uri())),
                "{name}, {status}: want a non-retryable error naming the other origin, got {err:?}"
            );
            let text = err.to_string();
            assert!(!text.contains(CANARY), "{name}: the key is in the error");
        }
        let reached = elsewhere.received_requests().await.unwrap();
        assert!(
            reached.is_empty(),
            "{status}: the other origin received {} request(s)",
            reached.len()
        );
        // Each provider asked once: a refused redirect is not retried here.
        assert_eq!(first.received_requests().await.unwrap().len(), 7);
    }
}

#[tokio::test]
async fn a_redirect_within_the_origin_is_still_followed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/moved/messages"))
        .mount(&server)
        .await;
    let sse: String = [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 1, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect();
    Mock::given(method("POST"))
        .and(path("/moved/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let message = send(
        &AnthropicProvider,
        ApiProtocol::AnthropicMessages,
        server.uri(),
    )
    .await
    .expect("a same-origin 307 is followed");
    let Message::Assistant { content, .. } = message else {
        panic!("not an assistant message");
    };
    assert!(matches!(&content[0], Content::Text { text } if text == "ok"));
}

#[cfg(feature = "decision")]
#[tokio::test]
async fn a_decision_backend_does_not_follow_a_redirect_to_another_origin() {
    use yoagent::decision::{DecisionModel, Question};
    let (first, elsewhere) = redirecting_pair(307).await;
    let err = DecisionModel::logprobs(first.uri(), "m")
        .with_api_key(CANARY)
        .ask(json!({"message": PROMPT}))
        .question("q", Question::noul("Is it urgent?"))
        .send()
        .await
        .expect_err("redirected");
    assert!(!err.is_retryable(), "{err:?}");
    assert!(elsewhere.received_requests().await.unwrap().is_empty());
    assert_eq!(first.received_requests().await.unwrap().len(), 1);
}
