pub mod anthropic;
pub mod azure_openai;
pub mod bedrock;
mod eventstream;
pub mod google;
pub mod google_vertex;
pub mod mock;
pub mod model;
pub mod openai_compat;
pub mod openai_responses;
pub mod prices;
pub mod registry;
mod responses_request;
mod responses_stream;
mod sigv4;
mod sse;
pub mod tool_args;
pub mod traits;
pub mod turn_hook;

pub use anthropic::AnthropicProvider;
pub use azure_openai::AzureOpenAiProvider;
pub use bedrock::BedrockProvider;
pub use google::GoogleProvider;
pub use google_vertex::GoogleVertexProvider;
pub use mock::MockProvider;
pub use model::{
    AnthropicCompat, ApiProtocol, ContextTier, CostConfig, GoogleCompat, ModelConfig, OpenAiCompat,
    ReasoningEffortCeiling,
};
pub use openai_compat::OpenAiCompatProvider;
pub use openai_responses::OpenAiResponsesProvider;
#[cfg(feature = "native")]
pub use prices::LivePrices;
pub use prices::{
    CacheOptions, CacheProblem, CachedPrices, FetchOptions, FetchReport, PriceChange, PriceEntry,
    PriceError, PriceOrigin, PriceSource, PriceTable, SkippedModel, DEFAULT_FETCH_TIMEOUT,
    PRICED_PROVIDERS, PRICES_ENV_VAR, PRICE_SCHEMA_VERSION,
};
pub(crate) use registry::resolve_api_key_or_warn;
pub use registry::{api_key_variables, resolve_api_key, ProviderRegistry};
pub use tool_args::{parse_tool_arguments, unparsed_tool_arguments, UNPARSED_ARGUMENTS_KEY};
pub use traits::*;
pub use turn_hook::TurnHookProvider;

/// The text of a tool result as one string: every non-empty text block, in
/// order, joined with `"\n"`; images and other blocks are left out (each
/// provider sends images its own way).
///
/// Providers that send a text-only tool result as a single string must send
/// all of it: a tool may answer in several blocks (MCP servers do — ESLint's
/// sends an intro line, the findings and a closing note), and taking only the
/// first block showed the model the intro and nothing else.
pub(crate) fn tool_result_text(content: &[crate::types::Content]) -> String {
    let texts: Vec<&str> = content
        .iter()
        .filter_map(|c| match c {
            crate::types::Content::Text { text } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    texts.join("\n")
}

#[cfg(test)]
mod tool_result_text_tests {
    use super::tool_result_text;
    use crate::types::Content;

    #[test]
    fn joins_every_text_block_in_order_and_skips_images() {
        let content = vec![
            Content::Text {
                text: "intro".into(),
            },
            Content::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            },
            Content::Text { text: "".into() },
            Content::Text {
                text: "findings".into(),
            },
        ];
        assert_eq!(tool_result_text(&content), "intro\nfindings");
        assert_eq!(tool_result_text(&[]), "");
        // An empty first block no longer hides the text after it.
        let empty_first = vec![
            Content::Text { text: "".into() },
            Content::Text { text: "x".into() },
        ];
        assert_eq!(tool_result_text(&empty_first), "x");
        // A single block is sent exactly as before.
        assert_eq!(
            tool_result_text(&[Content::Text {
                text: "only\n".into()
            }]),
            "only\n"
        );
    }
}
