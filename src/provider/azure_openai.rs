//! Azure OpenAI provider.
//!
//! Uses the OpenAI Responses API format on Azure's v1 surface:
//! `POST https://{resource}.openai.azure.com/openai/v1/responses`, no
//! `api-version` query, and the **deployment name** as `model` in the body.
//! Accepted `base_url` shapes: the resource endpoint, `.../openai`,
//! `.../openai/v1`, and the legacy `.../openai/deployments/{deployment}` form
//! (whose deployment then becomes `model`); see `docs/providers/azure-openai.md`.
//!
//! Auth: `api-key` header; for Microsoft Entra ID leave the key empty and set
//! `Authorization: Bearer ...` via `ModelConfig::headers`.
//!
//! The request body and the stream reader are shared with
//! [`OpenAiResponsesProvider`](super::OpenAiResponsesProvider)
//! (`responses_request.rs`); this file holds only what differs: the endpoint,
//! the auth header and the legacy deployment override of `model`.

use super::model::ApiProtocol;
use super::responses_request::{build_request_body, stream_response};
use super::traits::*;
use crate::types::*;
use tokio::sync::mpsc;
use tracing::{debug, info};

pub struct AzureOpenAiProvider;

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl StreamProvider for AzureOpenAiProvider {
    fn protocol(&self) -> Option<ApiProtocol> {
        Some(ApiProtocol::AzureOpenAiResponses)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        if config.output_schema.is_some() {
            tracing::warn!(
                "structured outputs are not yet wired for the Azure OpenAI provider; output_schema will be ignored"
            );
        }
        let model_config = config
            .model_config
            .as_ref()
            .ok_or_else(|| ProviderError::Other("ModelConfig required".into()))?;

        let endpoint = responses_endpoint(&model_config.base_url);
        let mut body = build_request_body(&config, ApiProtocol::AzureOpenAiResponses);
        if let Some(deployment) = &endpoint.deployment {
            // Legacy deployment-scoped base_url: the v1 surface names the
            // deployment in the body, not in the path.
            if *deployment != config.model {
                info!(
                    "Azure OpenAI: legacy deployment base_url; sending model={deployment:?} \
                     in place of the configured model {:?}",
                    config.model
                );
            }
            body["model"] = serde_json::json!(deployment);
        }
        debug!(
            "Azure OpenAI request: model={} url={}",
            body["model"], endpoint.url
        );

        let client = reqwest::Client::new();
        let mut request = client
            .post(&endpoint.url)
            .header("content-type", "application/json");
        if !config.api_key.is_empty() {
            request = request.header("api-key", &config.api_key);
        }

        for (k, v) in &model_config.headers {
            request = request.header(k, v);
        }

        stream_response(
            request.json(&body),
            "Azure OpenAI",
            ApiProtocol::AzureOpenAiResponses,
            &config,
            &model_config.provider,
            tx,
            cancel,
        )
        .await
    }
}

/// The resolved request target for an Azure `base_url`.
#[derive(Debug, PartialEq, Eq)]
struct Endpoint {
    /// Full Responses URL, e.g. `https://r.openai.azure.com/openai/v1/responses`.
    url: String,
    /// Deployment name taken from a legacy `/openai/deployments/{name}`
    /// base URL; it replaces `model` in the request body.
    deployment: Option<String>,
}

/// Map a `base_url` onto Azure's v1 Responses endpoint.
///
/// Azure documents the Responses API only at
/// `https://{resource}.openai.azure.com/openai/v1/responses` (GA, no
/// `api-version`), with the deployment name as `model`. Its REST specs never
/// placed `/responses` under `/openai/deployments/{id}`; the preview versions
/// (from `2025-03-01-preview`) served it at `/openai/responses`. Accepted
/// shapes:
///
/// | `base_url` | Request URL |
/// |---|---|
/// | `https://r.openai.azure.com` | `https://r.openai.azure.com/openai/v1/responses` |
/// | `https://r.openai.azure.com/openai` | same |
/// | `https://r.openai.azure.com/openai/v1` | same |
/// | `https://r.openai.azure.com/openai/deployments/d` | same, and `model` = `d` |
///
/// Trailing slashes are ignored; `*.services.ai.azure.com` works the same.
/// A query string or fragment is dropped before the path is read — a legacy
/// URL copied from the portal often carries `?api-version=2024-10-21`, which
/// would otherwise end up in the deployment name, and the v1 surface takes
/// no `api-version`.
fn responses_endpoint(base_url: &str) -> Endpoint {
    const DEPLOYMENTS: &str = "/openai/deployments";
    let path_end = base_url.find(['?', '#']).unwrap_or(base_url.len());
    if path_end < base_url.len() {
        info!(
            "Azure OpenAI: ignoring the query/fragment on base_url ({}); the v1 Responses \
             endpoint takes no api-version",
            &base_url[path_end..]
        );
    }
    let base = base_url[..path_end].trim_end_matches('/');
    let legacy = base
        .find(DEPLOYMENTS)
        .map(|i| (i, &base[i + DEPLOYMENTS.len()..]))
        .filter(|(_, rest)| rest.is_empty() || rest.starts_with('/'));
    if let Some((i, rest)) = legacy {
        let deployment = rest
            .trim_start_matches('/')
            .split('/')
            .next()
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        return Endpoint {
            url: format!("{}/openai/v1/responses", &base[..i]),
            deployment,
        };
    }
    let url = if base.ends_with("/openai/v1") {
        format!("{base}/responses")
    } else if base.ends_with("/openai") {
        format!("{base}/v1/responses")
    } else {
        format!("{base}/openai/v1/responses")
    };
    Endpoint {
        url,
        deployment: None,
    }
}

/// The shared Responses body, for tests that pin Azure's request.
#[cfg(test)]
fn build_azure_request_body(config: &StreamConfig) -> serde_json::Value {
    build_request_body(config, ApiProtocol::AzureOpenAiResponses)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(level: ThinkingLevel) -> StreamConfig {
        StreamConfig {
            model: "gpt-5.5".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: level,
            api_key: "key".into(),
            max_tokens: None,
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        }
    }

    #[test]
    fn thinking_level_sets_reasoning_effort() {
        let body = build_azure_request_body(&config(ThinkingLevel::Medium));
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn xhigh_and_max_clamp_to_high() {
        for level in [
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
            ThinkingLevel::High,
        ] {
            let body = build_azure_request_body(&config(level));
            assert_eq!(body["reasoning"]["effort"], "high");
        }
        let body = build_azure_request_body(&config(ThinkingLevel::Low));
        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn thinking_off_omits_reasoning() {
        let body = build_azure_request_body(&config(ThinkingLevel::Off));
        assert!(body["reasoning"].is_null());
    }

    /// An Azure deployment config carrying `compat`, the way the docs show:
    /// copied from the matching OpenAI preset.
    fn deployment(level: ThinkingLevel, compat_from: ModelConfig) -> StreamConfig {
        let mut mc = crate::provider::ModelConfig::custom(
            crate::provider::ApiProtocol::AzureOpenAiResponses,
            "azure",
            "https://r.openai.azure.com/openai/deployments/d",
            "gpt-6-sol",
            "GPT-6 Sol",
        );
        mc.compat = compat_from.compat;
        let mut c = config(level);
        c.model_config = Some(mc);
        c
    }

    use crate::provider::ModelConfig;

    #[test]
    fn every_base_url_shape_resolves_to_the_v1_responses_path() {
        let v1 = "https://r.openai.azure.com/openai/v1/responses";
        for base in [
            "https://r.openai.azure.com",
            "https://r.openai.azure.com/",
            "https://r.openai.azure.com/openai",
            "https://r.openai.azure.com/openai/",
            "https://r.openai.azure.com/openai/v1",
            "https://r.openai.azure.com/openai/v1/",
        ] {
            assert_eq!(
                responses_endpoint(base),
                Endpoint {
                    url: v1.into(),
                    deployment: None
                },
                "{base}"
            );
        }
        let foundry = responses_endpoint("https://r.services.ai.azure.com/openai/v1/");
        assert_eq!(
            foundry.url,
            "https://r.services.ai.azure.com/openai/v1/responses"
        );
    }

    #[test]
    fn legacy_deployment_base_url_moves_the_deployment_into_model() {
        for base in [
            "https://r.openai.azure.com/openai/deployments/my-gpt",
            "https://r.openai.azure.com/openai/deployments/my-gpt/",
        ] {
            assert_eq!(
                responses_endpoint(base),
                Endpoint {
                    url: "https://r.openai.azure.com/openai/v1/responses".into(),
                    deployment: Some("my-gpt".into()),
                },
                "{base}"
            );
        }
        // A query string or fragment is not part of the deployment name.
        for base in [
            "https://r.openai.azure.com/openai/deployments/my-gpt?api-version=2024-10-21",
            "https://r.openai.azure.com/openai/deployments/my-gpt/?api-version=2024-10-21",
            "https://r.openai.azure.com/openai/deployments/my-gpt#frag",
            "https://r.openai.azure.com/openai/deployments/my-gpt?a=b#frag",
        ] {
            assert_eq!(
                responses_endpoint(base),
                Endpoint {
                    url: "https://r.openai.azure.com/openai/v1/responses".into(),
                    deployment: Some("my-gpt".into()),
                },
                "{base}"
            );
        }
        // And on the non-legacy shapes it is dropped, not glued into the URL.
        assert_eq!(
            responses_endpoint("https://r.openai.azure.com/openai/v1?api-version=preview"),
            Endpoint {
                url: "https://r.openai.azure.com/openai/v1/responses".into(),
                deployment: None,
            }
        );
        // Near-miss: an empty deployment segment names nothing.
        let e = responses_endpoint("https://r.openai.azure.com/openai/deployments/");
        assert_eq!(e.url, "https://r.openai.azure.com/openai/v1/responses");
        assert_eq!(e.deployment, None);
    }

    #[test]
    fn compat_ceiling_is_honoured_and_off_omits_effort() {
        // Positive control: Azure used to clamp regardless of the model.
        let sol = || ModelConfig::gpt_6_sol();
        let body = build_azure_request_body(&deployment(ThinkingLevel::Off, sol()));
        assert!(body["reasoning"].is_null());
        for (level, want) in [
            (ThinkingLevel::XHigh, "xhigh"),
            (ThinkingLevel::Max, "max"),
            (ThinkingLevel::High, "high"),
        ] {
            let body = build_azure_request_body(&deployment(level, sol()));
            assert_eq!(body["reasoning"]["effort"], want, "{level:?}");
        }
        // Astra: max ceiling; Off omits the effort too.
        let body =
            build_azure_request_body(&deployment(ThinkingLevel::Off, ModelConfig::gpt_6_astra()));
        assert!(body["reasoning"].is_null());
    }

    #[test]
    fn a_model_config_without_compat_keeps_the_clamp() {
        // Near-miss: a ModelConfig present but carrying no compat behaves
        // exactly like no ModelConfig at all.
        for level in [
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
        ] {
            let mut with = deployment(level, ModelConfig::mock());
            assert!(with.model_config.as_ref().unwrap().compat.is_none());
            with.temperature = Some(0.2);
            let mut without = config(level);
            without.temperature = Some(0.2);
            assert_eq!(
                serde_json::to_string(&build_azure_request_body(&with)).unwrap(),
                serde_json::to_string(&build_azure_request_body(&without)).unwrap(),
                "{level:?}"
            );
        }
    }
}
