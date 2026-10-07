//! OpenAI Responses API provider.
//!
//! This is the newer OpenAI API that uses a different event format
//! from Chat Completions. It has first-class support for reasoning items.
//!
//! The request body and the stream reader are shared with
//! [`AzureOpenAiProvider`](super::AzureOpenAiProvider) (see
//! `responses_request.rs`); this file holds only what differs: the URL
//! (`{base_url}/responses`) and bearer auth.

use super::model::ApiProtocol;
use super::responses_request::{build_request_body, stream_response};
use super::traits::*;
use crate::types::*;
use tokio::sync::mpsc;
use tracing::debug;

pub struct OpenAiResponsesProvider;

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl StreamProvider for OpenAiResponsesProvider {
    fn protocol(&self) -> Option<ApiProtocol> {
        Some(ApiProtocol::OpenAiResponses)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        if config.output_schema.is_some() {
            tracing::warn!(
                "structured outputs are not yet wired for the OpenAI Responses provider; output_schema will be ignored"
            );
        }
        let model_config = config
            .model_config
            .as_ref()
            .ok_or_else(|| ProviderError::Other("ModelConfig required".into()))?;

        let url = format!("{}/responses", model_config.base_url);
        let body = build_request_body(&config, ApiProtocol::OpenAiResponses);
        debug!(
            "OpenAI Responses request: model={} url={}",
            config.model, url
        );

        let client = reqwest::Client::new();
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", config.api_key));

        for (k, v) in &model_config.headers {
            request = request.header(k, v);
        }

        stream_response(
            request.json(&body),
            "OpenAI Responses",
            ApiProtocol::OpenAiResponses,
            &config,
            &model_config.provider,
            tx,
            cancel,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::model::{ModelConfig, OpenAiCompat, ReasoningEffortCeiling};

    const ALL_LEVELS: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ];

    fn body(mc: &ModelConfig, level: ThinkingLevel) -> serde_json::Value {
        let mut config = StreamConfig::new(mc.id.clone(), "key");
        config.messages = vec![Message::user("hi")];
        config.thinking_level = level;
        config.temperature = Some(0.5);
        config.model_config = Some(mc.clone());
        build_request_body(&config, ApiProtocol::OpenAiResponses)
    }

    fn effort(mc: &ModelConfig, level: ThinkingLevel) -> serde_json::Value {
        body(mc, level)["reasoning"]["effort"].clone()
    }

    #[test]
    fn without_compat_the_body_has_the_default_effort_ladder() {
        // Near-miss guard: `openai_responses` carries `compat: None`, so the
        // whole body is pinned — Off omits `reasoning`, XHigh/Max clamp to
        // `high`.
        let mc = ModelConfig::openai_responses("gpt-5.5", "GPT-5.5");
        assert!(mc.compat.is_none());
        for level in ALL_LEVELS {
            let expected_effort = match level {
                ThinkingLevel::Off => None,
                ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
                ThinkingLevel::Medium => Some("medium"),
                _ => Some("high"),
            };
            let got = body(&mc, level);
            // `openai_responses` declares a reasoning model, so encrypted
            // reasoning is requested at every level; the empty system prompt
            // derives no cache key.
            let mut expected = serde_json::json!({
                "model": "gpt-5.5",
                "stream": true,
                "input": [{"role": "user", "content": "hi"}],
                "include": ["reasoning.encrypted_content"],
                "temperature": 0.5,
            });
            if let Some(e) = expected_effort {
                expected["reasoning"] = serde_json::json!({"effort": e});
            }
            assert_eq!(
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(&expected).unwrap(),
                "{level:?}"
            );
        }
    }

    #[test]
    fn compat_ceiling_is_honoured() {
        // Positive control: the Responses builder used to ignore
        // `model_config.compat` entirely.
        let mut mc = ModelConfig::openai_responses("gpt-5.4", "GPT-5.4");
        mc.compat = Some(OpenAiCompat {
            max_reasoning_effort: ReasoningEffortCeiling::XHigh,
            ..Default::default()
        });
        assert_eq!(effort(&mc, ThinkingLevel::XHigh), "xhigh");
        assert_eq!(effort(&mc, ThinkingLevel::Max), "xhigh");
        assert_eq!(effort(&mc, ThinkingLevel::High), "high");
        assert!(body(&mc, ThinkingLevel::Off).get("reasoning").is_none());
    }

    #[test]
    fn gpt_6_astra_reaches_max_and_never_sends_none() {
        let mc = ModelConfig::gpt_6_astra();
        assert_eq!(effort(&mc, ThinkingLevel::Max), "max");
        assert_eq!(effort(&mc, ThinkingLevel::XHigh), "xhigh");
        assert_eq!(effort(&mc, ThinkingLevel::High), "high");
        assert_eq!(effort(&mc, ThinkingLevel::Medium), "medium");
        // GPT-6 has no `minimal`.
        assert_eq!(effort(&mc, ThinkingLevel::Minimal), "low");
        // `none` is an HTTP 400 on Astra: Off omits the effort instead.
        assert!(body(&mc, ThinkingLevel::Off).get("reasoning").is_none());
    }

    #[test]
    fn gpt_6_sol_and_luna_omit_effort_for_off() {
        for mc in [ModelConfig::gpt_6_sol(), ModelConfig::gpt_6_luna()] {
            assert!(body(&mc, ThinkingLevel::Off).get("reasoning").is_none());
            assert_eq!(effort(&mc, ThinkingLevel::Max), "max", "{}", mc.id);
            assert_eq!(effort(&mc, ThinkingLevel::XHigh), "xhigh", "{}", mc.id);
            assert_eq!(effort(&mc, ThinkingLevel::Low), "low", "{}", mc.id);
        }
    }

    #[test]
    fn off_bodies_are_byte_identical_to_the_uncapped_body_for_every_preset() {
        // `Off` means "send nothing": whatever the preset's ceiling, the body
        // equals the pre-#176 one (a compat-less config's body with the same
        // model id), with no `reasoning` key.
        for mc in [
            ModelConfig::gpt_6_astra(),
            ModelConfig::gpt_6_sol(),
            ModelConfig::gpt_6_luna(),
            ModelConfig::gpt_5_5(),
        ] {
            let mut legacy = mc.clone();
            legacy.compat = None;
            let got = body(&mc, ThinkingLevel::Off);
            assert!(got.get("reasoning").is_none(), "{}", mc.id);
            assert_eq!(
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(&body(&legacy, ThinkingLevel::Off)).unwrap(),
                "{}",
                mc.id
            );
        }
    }
}
