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

/// Merge each message into the one before it when both have the same `role`,
/// appending its `key` array (`"content"` on Bedrock, `"parts"` on Gemini).
///
/// Bedrock's Converse and Gemini require user and assistant/model turns to
/// alternate, and a turn's tool results to arrive together, in one message.
/// The loop keeps one `Message::ToolResult` per call (and may put a steering
/// message, a stop marker or a compaction summary right after them), so these
/// builders produce one wire message per result. Merging keeps every block in
/// order: the results first, in call order, then whatever followed them.
///
/// A message whose `key` array is empty is dropped first, of either role:
/// both APIs reject an empty message, and a turn that errored, was refused or
/// was cancelled with no content (the loop's error message is one empty text
/// block, which the builders filter), or whose only content is thinking the
/// provider does not replay, would otherwise be one. Its neighbours then
/// merge, so the roles still alternate (#369). A first message dropped this
/// way can leave an assistant turn first, which Bedrock rejects — as it
/// rejected the empty message before.
pub(crate) fn merge_same_role(
    messages: Vec<serde_json::Value>,
    key: &str,
) -> Vec<serde_json::Value> {
    let mut merged: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
    for message in messages {
        if message
            .get(key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            if last.get("role") == message.get("role") {
                let from = message.get(key).and_then(serde_json::Value::as_array);
                if let (Some(into), Some(from)) = (
                    last.get_mut(key).and_then(serde_json::Value::as_array_mut),
                    from,
                ) {
                    into.extend(from.iter().cloned());
                    continue;
                }
            }
        }
        merged.push(message);
    }
    merged
}

/// A conversation with a parallel tool call: the user's request, an assistant
/// turn calling two tools, both results, then a steering message — four user
/// role messages in a row once results are user turns on the wire.
#[cfg(test)]
pub(crate) fn parallel_tool_conversation() -> Vec<crate::types::Message> {
    use crate::types::{Content, Message, StopReason, Usage};
    let result = |id: &str, name: &str, text: &str| Message::ToolResult {
        tool_call_id: id.into(),
        tool_name: name.into(),
        content: vec![Content::Text { text: text.into() }],
        is_error: false,
        timestamp: 0,
    };
    vec![
        Message::user("look at both files"),
        Message::assistant(
            vec![
                Content::tool_call("call-a", "read_file", serde_json::json!({"path": "a.rs"})),
                Content::tool_call("call-b", "read_file", serde_json::json!({"path": "b.rs"})),
            ],
            StopReason::ToolUse,
            "m",
            "p",
            Usage::default(),
        ),
        result("call-a", "read_file", "fn a() {}"),
        result("call-b", "read_file", "fn b() {}"),
        Message::user("also check c.rs"),
    ]
}

#[cfg(test)]
mod merge_same_role_tests {
    use super::merge_same_role;
    use serde_json::json;

    /// An empty message is dropped, first, in the middle or last, and its
    /// neighbours merge: an assistant turn that errored with no content
    /// between two user turns leaves one user message (#369).
    #[test]
    fn empty_messages_are_dropped_and_their_neighbours_merge() {
        let merged = merge_same_role(
            vec![
                json!({"role": "user", "parts": []}),
                json!({"role": "user", "parts": [{"text": "1"}]}),
                json!({"role": "model", "parts": []}),
                json!({"role": "user", "parts": [{"text": "2"}]}),
                json!({"role": "model", "parts": [{"text": "3"}]}),
                json!({"role": "model", "parts": []}),
            ],
            "parts",
        );
        assert_eq!(
            merged,
            vec![
                json!({"role": "user", "parts": [{"text": "1"}, {"text": "2"}]}),
                json!({"role": "model", "parts": [{"text": "3"}]}),
            ]
        );
    }

    /// An empty user message is dropped too, and the model turns around it
    /// then merge (#369).
    #[test]
    fn an_empty_user_message_is_dropped() {
        let merged = merge_same_role(
            vec![
                json!({"role": "user", "content": [{"text": "a"}]}),
                json!({"role": "assistant", "content": [{"text": "x"}]}),
                json!({"role": "user", "content": []}),
                json!({"role": "assistant", "content": [{"text": "y"}]}),
            ],
            "content",
        );
        assert_eq!(
            merged,
            vec![
                json!({"role": "user", "content": [{"text": "a"}]}),
                json!({"role": "assistant", "content": [{"text": "x"}, {"text": "y"}]}),
            ]
        );
    }

    #[test]
    fn adjacent_same_role_messages_become_one_in_order() {
        let merged = merge_same_role(
            vec![
                json!({"role": "user", "content": [{"text": "1"}]}),
                json!({"role": "assistant", "content": [{"text": "2"}]}),
                json!({"role": "user", "content": []}),
                json!({"role": "user", "content": [{"text": "3"}]}),
                json!({"role": "user", "content": [{"text": "4"}, {"text": "5"}]}),
                json!({"role": "user", "content": []}),
                json!({"role": "assistant", "content": [{"text": "6"}]}),
                json!({"role": "assistant", "content": [{"text": "7"}]}),
                // Without the key: kept as it is, never given a `null` one.
                json!({"role": "user"}),
                json!({"role": "user", "content": [{"text": "8"}]}),
            ],
            "content",
        );
        assert_eq!(
            merged,
            vec![
                json!({"role": "user", "content": [{"text": "1"}]}),
                json!({"role": "assistant", "content": [{"text": "2"}]}),
                json!({"role": "user", "content": [{"text": "3"}, {"text": "4"}, {"text": "5"}]}),
                json!({"role": "assistant", "content": [{"text": "6"}, {"text": "7"}]}),
                json!({"role": "user"}),
                json!({"role": "user", "content": [{"text": "8"}]}),
            ]
        );
    }
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
