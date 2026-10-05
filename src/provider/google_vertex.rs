//! Google Vertex AI provider.
//!
//! Similar to Google Generative AI but uses OAuth2 authentication
//! and a different base URL pattern with project/location.
//!
//! The API key in StreamConfig is expected to be an OAuth2 access token.
//!
//! Prompt caching behaves exactly as in [`super::google`], including the
//! reasoning for why explicit `CachedContent` is deliberately not wired — see
//! that module's docs rather than re-deriving it here.
//! Callers are responsible for obtaining the token (e.g., via service account JWT).

use super::model::ModelConfig;
use super::traits::*;
use crate::types::*;
use tokio::sync::mpsc;

pub struct GoogleVertexProvider;

impl GoogleVertexProvider {
    /// Build the Vertex AI URL from model config.
    /// Expects base_url in format: `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models`
    fn vertex_url(model_config: &ModelConfig, model: &str) -> String {
        format!(
            "{}/{}:streamGenerateContent?alt=sse",
            model_config.base_url, model
        )
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl StreamProvider for GoogleVertexProvider {
    fn protocol(&self) -> Option<crate::provider::ApiProtocol> {
        Some(crate::provider::ApiProtocol::GoogleVertex)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        if config.output_schema.is_some() {
            tracing::warn!(
                "structured outputs are not yet wired for the Google Vertex provider; output_schema will be ignored"
            );
        }
        let model_config = config
            .model_config
            .as_ref()
            .ok_or_else(|| ProviderError::Other("ModelConfig required".into()))?;

        // Vertex's URL pattern differs from the Gemini API's.
        let vertex_url = Self::vertex_url(model_config, &config.model);

        // Create a modified model config with the Vertex URL pattern
        let mut vertex_model = model_config.clone();
        // Vertex authenticates with an OAuth2 Bearer token, not a Gemini API
        // key.
        vertex_model.headers.insert(
            "authorization".to_string(),
            format!("Bearer {}", config.api_key),
        );

        // Build request body same as Google (same content format)
        let body = build_vertex_request_body(&config);

        let client = reqwest::Client::new();
        let mut request = client
            .post(&vertex_url)
            .header("content-type", "application/json");

        for (k, v) in &vertex_model.headers {
            request = request.header(k, v);
        }

        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = parse_retry_after(response.headers());
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::classify_with_retry_after(
                status.as_u16(),
                &format!("Vertex AI error {}: {}", status, body),
                retry_after,
            ));
        }

        // Same SSE format as the Gemini API: one parser for both.
        super::google::parse_stream(response, &config.model, &model_config.provider, tx, cancel)
            .await
    }
}

/// Build the request body for Vertex AI (same format as Google GenAI).
fn build_vertex_request_body(config: &StreamConfig) -> serde_json::Value {
    // Same format as Google GenAI
    let mut contents: Vec<serde_json::Value> = Vec::new();

    for msg in &config.messages {
        match msg {
            Message::User { content, .. } => {
                let parts: Vec<serde_json::Value> = content
                    .iter()
                    .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(serde_json::json!({"text": text})),
                        Content::Image { data, mime_type } => Some(serde_json::json!({
                            "inlineData": {"mimeType": mime_type, "data": data},
                        })),
                        _ => None,
                    })
                    .collect();
                contents.push(serde_json::json!({"role": "user", "parts": parts}));
            }
            Message::Assistant { content, .. } => {
                let parts: Vec<serde_json::Value> = content
                    .iter()
                    .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(serde_json::json!({"text": text})),
                        Content::ToolCall {
                            name,
                            arguments,
                            provider_metadata,
                            ..
                        } => {
                            let mut part = serde_json::json!({
                                "functionCall": {"name": name, "args": arguments},
                            });
                            if let Some(sig) = provider_metadata
                                .as_ref()
                                .and_then(|m| m.get("thought_signature"))
                                .and_then(|v| v.as_str())
                            {
                                part["thoughtSignature"] = serde_json::json!(sig);
                            }
                            Some(part)
                        }
                        _ => None,
                    })
                    .collect();
                contents.push(serde_json::json!({"role": "model", "parts": parts}));
            }
            Message::ToolResult {
                tool_name, content, ..
            } => {
                let text = content
                    .iter()
                    .find_map(|c| match c {
                        Content::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();

                let mut parts = vec![serde_json::json!({
                    "functionResponse": {"name": tool_name, "response": {"result": text}}
                })];

                for c in content {
                    if let Content::Image { data, mime_type } = c {
                        parts.push(serde_json::json!({
                            "inlineData": {"mimeType": mime_type, "data": data},
                        }));
                    }
                }

                contents.push(serde_json::json!({
                    "role": "user",
                    "parts": parts,
                }));
            }
        }
    }

    let mut body = serde_json::json!({"contents": contents});

    if !config.system_prompt.is_empty() {
        body["systemInstruction"] = serde_json::json!({"parts": [{"text": config.system_prompt}]});
    }

    let mut gen_config = serde_json::json!({});
    if let Some(max) = config.max_tokens {
        gen_config["maxOutputTokens"] = serde_json::json!(max);
    }
    if let Some(temp) = config.temperature {
        gen_config["temperature"] = serde_json::json!(temp);
    }
    // Thinking: same thinkingConfig as the Gemini API (level on 3+, budget on 2.x).
    if let Some(thinking) = super::google::gemini_thinking_config(config) {
        gen_config["thinkingConfig"] = thinking;
    }
    if gen_config != serde_json::json!({}) {
        body["generationConfig"] = gen_config;
    }

    if !config.tools.is_empty() {
        let declarations: Vec<serde_json::Value> = config
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["tools"] = serde_json::json!([{"functionDeclarations": declarations}]);
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(level: ThinkingLevel) -> StreamConfig {
        StreamConfig {
            model: "gemini-2.5-pro".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: level,
            api_key: "token".into(),
            max_tokens: None,
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        }
    }

    #[test]
    fn tool_call_thought_signature_is_replayed() {
        // Parity with the Gemini API provider: signatures captured into
        // provider_metadata must ride back on functionCall parts.
        let mut c = config(ThinkingLevel::Off);
        c.messages = vec![
            Message::user("go"),
            Message::assistant(
                vec![Content::ToolCall {
                    id: "vertex-fc-0".into(),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({"city": "Paris"}),
                    provider_metadata: Some(serde_json::json!({"thought_signature": "sig-9"})),
                }],
                StopReason::ToolUse,
                "m",
                "vertex",
                Usage::default(),
            ),
        ];
        let body = build_vertex_request_body(&c);
        let part = &body["contents"][1]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "get_weather");
        assert_eq!(part["thoughtSignature"], "sig-9");
    }

    #[test]
    fn thinking_level_sets_thinking_config() {
        let body = build_vertex_request_body(&config(ThinkingLevel::High));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            24576
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn xhigh_and_max_clamp_to_the_flash_budget_ceiling() {
        for level in [ThinkingLevel::XHigh, ThinkingLevel::Max] {
            let body = build_vertex_request_body(&config(level));
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
                24576
            );
        }
        let body = build_vertex_request_body(&config(ThinkingLevel::Medium));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            8192
        );
    }

    #[test]
    fn thinking_off_omits_thinking_config() {
        let body = build_vertex_request_body(&config(ThinkingLevel::Off));
        assert!(body["generationConfig"]["thinkingConfig"].is_null());
    }

    fn vertex_thinking(model: &str, level: ThinkingLevel) -> serde_json::Value {
        let mut c = config(level);
        c.model = model.into();
        build_vertex_request_body(&c)["generationConfig"]["thinkingConfig"].clone()
    }

    #[test]
    fn gemini_3_on_vertex_sends_thinking_level_not_budget() {
        // Vertex ids arrive bare or as full publisher resource paths.
        for model in [
            "gemini-3.1-pro-preview",
            "publishers/google/models/gemini-3.1-pro-preview",
            "projects/p/locations/global/publishers/google/models/gemini-3.1-pro-preview",
        ] {
            assert_eq!(
                vertex_thinking(model, ThinkingLevel::High),
                serde_json::json!({"thinkingLevel": "HIGH", "includeThoughts": true}),
                "{model}"
            );
            // Off sends nothing: 3.1 Pro then thinks at its default (HIGH).
            assert!(
                vertex_thinking(model, ThinkingLevel::Off).is_null(),
                "{model}"
            );
            // 3.1 Pro has no MINIMAL rung.
            assert_eq!(
                vertex_thinking(model, ThinkingLevel::Minimal)["thinkingLevel"],
                "LOW",
                "{model}"
            );
        }
        // Image models on Vertex: only the levels the Vertex table lists.
        assert_eq!(
            vertex_thinking("gemini-3.1-flash-image", ThinkingLevel::Low)["thinkingLevel"],
            "MINIMAL"
        );
        assert_eq!(
            vertex_thinking("gemini-3-pro-image", ThinkingLevel::Low)["thinkingLevel"],
            "HIGH"
        );
        assert!(vertex_thinking("gemini-3.1-flash-image", ThinkingLevel::Off).is_null());
        assert_eq!(
            vertex_thinking(
                "projects/p/locations/global/publishers/google/models/gemini-3.5-flash-lite",
                ThinkingLevel::Minimal
            )["thinkingLevel"],
            "MINIMAL"
        );
        // Positive control: the 2.5 path id on the same route keeps the budget.
        assert_eq!(
            vertex_thinking(
                "projects/p/locations/global/publishers/google/models/gemini-2.5-pro",
                ThinkingLevel::High
            ),
            serde_json::json!({"thinkingBudget": 24576, "includeThoughts": true})
        );
    }

    #[test]
    fn vertex_honours_the_google_compat_override() {
        let mut c = config(ThinkingLevel::Medium);
        c.model = "gemini-3.8-flash".into();
        let mut mc = ModelConfig::custom(
            crate::provider::ApiProtocol::GoogleVertex,
            "vertex",
            "https://example.invalid",
            "gemini-3.8-flash",
            "G",
        );
        mc.google = Some(crate::provider::GoogleCompat::force_thinking_budget());
        c.model_config = Some(mc);
        assert_eq!(
            build_vertex_request_body(&c)["generationConfig"]["thinkingConfig"],
            serde_json::json!({"thinkingBudget": 8192, "includeThoughts": true})
        );
    }
}
