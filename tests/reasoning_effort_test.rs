//! Reasoning effort on the wire (#176): what `ThinkingLevel` becomes in the
//! request body each OpenAI-shaped provider actually sends, read back from a
//! mock server rather than from the private body builders.
//!
//! The unit tests next to each builder cover the mapping table; these cover
//! the plumbing — that the capability on `ModelConfig::compat` survives the
//! trip through `StreamConfig::model_config` into all three providers.

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::{
    AzureOpenAiProvider, ModelConfig, OpenAiCompatProvider, OpenAiResponsesProvider, StreamConfig,
    StreamProvider,
};
use yoagent::types::*;

#[derive(Clone, Copy, Debug)]
enum Which {
    ChatCompletions,
    Responses,
    Azure,
}

fn responses_sse() -> String {
    let events = [
        json!({"type": "response.created", "response": {"id": "r", "status": "in_progress", "output": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "ok"}),
        json!({"type": "response.completed", "response": {"id": "r", "status": "completed",
               "usage": {"input_tokens": 3, "output_tokens": 1}}}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn chat_sse() -> String {
    [
        json!({"choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_string()))
    .collect()
}

/// Send one request and return the JSON body the server received.
async fn sent_body(which: Which, mut mc: ModelConfig, level: ThinkingLevel) -> Value {
    let server = MockServer::start().await;
    let sse = match which {
        Which::ChatCompletions => chat_sse(),
        Which::Responses | Which::Azure => responses_sse(),
    };
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&server)
        .await;
    mc.base_url = server.uri();
    let mut config = StreamConfig::new(mc.id.clone(), "test-key");
    config.messages = vec![Message::user("hi")];
    config.thinking_level = level;
    config.model_config = Some(mc);
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let result = match which {
        Which::ChatCompletions => OpenAiCompatProvider.stream(config, tx, cancel).await,
        Which::Responses => OpenAiResponsesProvider.stream(config, tx, cancel).await,
        Which::Azure => AzureOpenAiProvider.stream(config, tx, cancel).await,
    };
    result.unwrap_or_else(|e| panic!("{which:?}: {e}"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    serde_json::from_slice(&requests[0].body).unwrap()
}

/// The effort a body carries, whichever field shape its protocol uses.
fn effort(which: Which, body: &Value) -> Option<String> {
    let v = match which {
        Which::ChatCompletions => body.get("reasoning_effort"),
        Which::Responses | Which::Azure => body.get("reasoning").and_then(|r| r.get("effort")),
    };
    v.map(|v| v.as_str().unwrap().to_string())
}

/// An Azure deployment of `preset`'s model, carrying the preset's compat —
/// the documented way to give a deployment an effort capability.
fn azure(preset: ModelConfig) -> ModelConfig {
    let mut mc = ModelConfig::custom(
        yoagent::provider::ApiProtocol::AzureOpenAiResponses,
        "azure",
        "http://unused",
        preset.id.clone(),
        preset.name.clone(),
    );
    mc.compat = preset.compat;
    mc
}

#[tokio::test]
async fn gpt_6_sol_off_sends_none_and_max_sends_max_on_responses_and_azure() {
    for (which, mc) in [
        (Which::Responses, ModelConfig::gpt_6_sol()),
        (Which::Azure, azure(ModelConfig::gpt_6_sol())),
    ] {
        for (level, want) in [
            (ThinkingLevel::Off, "none"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::XHigh, "xhigh"),
            (ThinkingLevel::Max, "max"),
        ] {
            let body = sent_body(which, mc.clone(), level).await;
            assert_eq!(
                effort(which, &body).as_deref(),
                Some(want),
                "{which:?} {level:?}"
            );
        }
    }
}

#[tokio::test]
async fn gpt_6_astra_off_omits_effort_on_responses_and_azure() {
    for (which, mc) in [
        (Which::Responses, ModelConfig::gpt_6_astra()),
        (Which::Azure, azure(ModelConfig::gpt_6_astra())),
    ] {
        let body = sent_body(which, mc.clone(), ThinkingLevel::Off).await;
        assert_eq!(
            effort(which, &body),
            None,
            "{which:?}: none is a 400 on Astra"
        );
        let body = sent_body(which, mc, ThinkingLevel::Max).await;
        assert_eq!(effort(which, &body).as_deref(), Some("max"), "{which:?}");
    }
}

#[tokio::test]
async fn gpt_5_5_on_chat_completions_reaches_xhigh_and_sends_none() {
    let mc = ModelConfig::gpt_5_5();
    for (level, want) in [
        (ThinkingLevel::Off, "none"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
        (ThinkingLevel::Max, "xhigh"),
    ] {
        let body = sent_body(Which::ChatCompletions, mc.clone(), level).await;
        assert_eq!(
            effort(Which::ChatCompletions, &body).as_deref(),
            Some(want),
            "{level:?}"
        );
    }
}

#[tokio::test]
async fn configs_without_the_capability_keep_the_high_clamp() {
    // Near-miss: the generic constructors declare nothing, so every
    // provider still clamps XHigh/Max to `high` and omits Off.
    for (which, mc) in [
        (
            Which::ChatCompletions,
            ModelConfig::openai("gpt-5.5", "GPT-5.5"),
        ),
        (
            Which::Responses,
            ModelConfig::openai_responses("gpt-5.5", "GPT-5.5"),
        ),
        (
            Which::Azure,
            azure(ModelConfig::openai_responses("gpt-5.5", "GPT-5.5")),
        ),
    ] {
        for level in [ThinkingLevel::XHigh, ThinkingLevel::Max] {
            let body = sent_body(which, mc.clone(), level).await;
            assert_eq!(
                effort(which, &body).as_deref(),
                Some("high"),
                "{which:?} {level:?}"
            );
        }
        let body = sent_body(which, mc.clone(), ThinkingLevel::Off).await;
        assert_eq!(effort(which, &body), None, "{which:?}");
    }
}
