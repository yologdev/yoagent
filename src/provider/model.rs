//! Model configuration and provider compatibility flags.

use crate::types::ThinkingLevel;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which API protocol a model uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApiProtocol {
    AnthropicMessages,
    OpenAiCompletions,
    OpenAiResponses,
    AzureOpenAiResponses,
    GoogleGenerativeAi,
    GoogleVertex,
    BedrockConverseStream,
}

impl std::fmt::Display for ApiProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnthropicMessages => write!(f, "anthropic_messages"),
            Self::OpenAiCompletions => write!(f, "openai_completions"),
            Self::OpenAiResponses => write!(f, "openai_responses"),
            Self::AzureOpenAiResponses => write!(f, "azure_openai_responses"),
            Self::GoogleGenerativeAi => write!(f, "google_generative_ai"),
            Self::GoogleVertex => write!(f, "google_vertex"),
            Self::BedrockConverseStream => write!(f, "bedrock_converse_stream"),
        }
    }
}

/// Cost per million tokens (input/output).
///
/// # These are a snapshot, not an authority
///
/// The built-in presets carry rates verified against the vendor's published
/// pricing on the date noted at each constructor. Vendors reprice, and a
/// compiled-in number cannot notice — `claude_sonnet_5` shipped Sonnet 4.6's
/// rates across 18 releases, v0.9.0 through v0.16.5, overstating every
/// `cost_usd` for that model by 50%, and nothing detected it.
///
/// # Context tiers
///
/// Some vendors charge more above a prompt-size threshold. Set
/// [`context_tier`](Self::context_tier) and `cost_usd` selects by the request's
/// prompt tokens (`input + cache_read + cache_write`).
///
/// The OpenAI presets set one at 272K: `gpt_5_5`, `gpt_6_astra`,
/// `gpt_6_sol` and `gpt_6_luna` (checked 2026-09-25; `gpt_5_5`'s long-band
/// cache-read rate is unverified — see its docs). The Anthropic presets do
/// not: Anthropic states that 4.6+ models bill the full 1M window at
/// standard rates. Meta's page says there is no long-context premium; Haiku
/// 4.5 is flat because its window is 200K.
///
/// Note the derivation: prompt size is `input + cache_read + cache_write`, which
/// holds only where the provider subtracts cached tokens out of `input`.
/// `bedrock.rs` populates neither cache field, so a heavily-cached prompt reads
/// small there and would select the cheap tier. Fix that before tiering a model
/// Bedrock serves.
///
/// `tests/price_audit.rs` now diffs every preset against models.dev; run it
/// before a release:
///
/// ```text
/// cargo test --test price_audit -- --ignored --nocapture
/// ```
///
/// `ModelConfig::cost` is a public field and `CostConfig` is `Deserialize`, so
/// a caller never has to wait for a release — override it for a negotiated
/// rate, or load rates from configuration:
///
/// ```
/// # use yoagent::provider::{CostConfig, ModelConfig};
/// // A named preset is priced; adjust one rate and keep the rest.
/// let mut config = ModelConfig::claude_sonnet_5();
/// config
///     .cost
///     .get_or_insert_with(CostConfig::default)
///     .input_per_million = 1.80; // your negotiated rate
/// assert_eq!(config.cost.as_ref().unwrap().output_per_million, 10.0);
///
/// // Generic constructors carry no price (`cost: None`). `get_or_insert_with`
/// // works here too — `if let Some(c) = config.cost.as_mut()` would silently
/// // do nothing — but set every rate you pay, since the rest start at zero:
/// let mut deepseek = ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash");
/// assert!(deepseek.cost.is_none());
/// deepseek.cost = Some(CostConfig::new(0.15, 0.60).with_cache_read(0.003));
/// assert_eq!(deepseek.cost.as_ref().unwrap().input_per_million, 0.15);
///
/// // A model you run for free is `Some` with zero rates, and costs $0:
/// let mut local = ModelConfig::local("http://localhost:1234/v1", "qwen3");
/// local.cost = Some(CostConfig::new(0.0, 0.0));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CostConfig {
    pub input_per_million: f64,
    pub output_per_million: f64,
    #[serde(default)]
    pub cache_read_per_million: f64,
    #[serde(default)]
    pub cache_write_per_million: f64,
    /// Rates that replace the above once a request's prompt exceeds a
    /// threshold, ascending by threshold. Empty means one flat rate at every
    /// size.
    ///
    /// A `Vec` rather than a single tier because vendors publish multi-step
    /// schedules and models.dev already represents this as an array — making
    /// it one tier would buy a second breaking release the first time a
    /// three-tier model appears, and it would break the serde key as well as
    /// the field type, invalidating every persisted config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_tiers: Vec<ContextTier>,
}

/// Higher rates charged above a context threshold.
///
/// OpenAI prices gpt-5.5 at $5/$30 below 272K prompt tokens and $10/$45 above
/// it, and the GPT-6 models at 2x input and cache rates and 1.5x output above
/// the same line — published as *columns* on the same pricing row, which is
/// easy to miss if you go looking for a second row. A flat `CostConfig` under-bills those
/// requests by 2x on input, and this crate's whole compaction subsystem exists
/// to run agents at high context, so the case is central rather than exotic.
///
/// The threshold is compared against the request's **prompt** tokens —
/// `input + cache_read + cache_write` — not the total including output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContextTier {
    /// Prompt tokens above which the tier rates apply.
    pub above_prompt_tokens: u64,
    pub input_per_million: f64,
    pub output_per_million: f64,
    #[serde(default)]
    pub cache_read_per_million: f64,
    #[serde(default)]
    pub cache_write_per_million: f64,
}

impl ContextTier {
    /// A tier's threshold and the two rates every tier has.
    ///
    /// Cache rates are added with [`with_cache_read`](Self::with_cache_read)
    /// and [`with_cache_write`](Self::with_cache_write), mirroring
    /// [`CostConfig::new`] and for the same reason. The previous shape took
    /// cache-read positionally and silently zeroed cache-write, which would
    /// have dropped an Anthropic-style tier's $12.50/M cache writes to $0
    /// above the threshold.
    ///
    /// `above_prompt_tokens` is exclusive: a prompt exactly at the threshold
    /// stays on the band below, matching how vendors publish `>272K`.
    pub fn new(above_prompt_tokens: u64, input_per_million: f64, output_per_million: f64) -> Self {
        debug_assert!(
            above_prompt_tokens > 0,
            "a tier at 0 applies to every request, making the base rates dead code"
        );
        Self {
            above_prompt_tokens,
            input_per_million,
            output_per_million,
            cache_read_per_million: 0.0,
            cache_write_per_million: 0.0,
        }
    }

    /// Rate for reading a cached prompt prefix above this threshold.
    pub fn with_cache_read(mut self, per_million: f64) -> Self {
        self.cache_read_per_million = per_million;
        self
    }

    /// Rate for writing a prompt prefix into the cache above this threshold.
    pub fn with_cache_write(mut self, per_million: f64) -> Self {
        self.cache_write_per_million = per_million;
        self
    }

    /// Whether this tier sets any rate. Mirrors [`CostConfig::is_configured`].
    pub fn is_configured(&self) -> bool {
        self.input_per_million != 0.0
            || self.output_per_million != 0.0
            || self.cache_read_per_million != 0.0
            || self.cache_write_per_million != 0.0
    }
}

impl CostConfig {
    /// The two rates every priced model has.
    ///
    /// Cache rates are set with [`with_cache_read`](Self::with_cache_read) and
    /// [`with_cache_write`](Self::with_cache_write) rather than as positional
    /// arguments. Four same-typed `f64`s in a row is a transposition waiting to
    /// happen, and no vendor publishes them in one order: Anthropic lists
    /// input / cache write / cache read / output, OpenAI lists input / cached
    /// input / output. Transcribing top-to-bottom from either page produced a
    /// wrong-but-compiling config, and `is_configured` returns `true` for a
    /// transposed one, so every downstream guard passes. That is exactly how
    /// `claude_sonnet_5` billed 50% high for 18 releases. Two arguments still
    /// transpose, but output is always dearer than input, so the mistake is
    /// visible.
    ///
    /// `CostConfig` is `#[non_exhaustive]`, so downstream crates build it here
    /// rather than with a struct literal.
    pub fn new(input_per_million: f64, output_per_million: f64) -> Self {
        Self {
            input_per_million,
            output_per_million,
            cache_read_per_million: 0.0,
            cache_write_per_million: 0.0,
            context_tiers: Vec::new(),
        }
    }

    /// Rate for reading a cached prompt prefix.
    pub fn with_cache_read(mut self, per_million: f64) -> Self {
        self.cache_read_per_million = per_million;
        self
    }

    /// Rate for writing a prompt prefix into the cache.
    pub fn with_cache_write(mut self, per_million: f64) -> Self {
        self.cache_write_per_million = per_million;
        self
    }

    /// Charge higher rates above a prompt-size threshold.
    ///
    /// Repeatable. Tiers are kept sorted by threshold so `cost_usd` can take
    /// the last one the prompt clears, and so declaration order cannot change
    /// what a config costs.
    pub fn with_context_tier(mut self, tier: ContextTier) -> Self {
        self.context_tiers.push(tier);
        self.context_tiers.sort_by_key(|t| t.above_prompt_tokens);
        self
    }

    /// Whether any rate is set.
    ///
    /// Not a "known price" check. In memory an all-zero `CostConfig` means
    /// **free**, and unknown pricing is `ModelConfig::cost == None`. This
    /// predicate matters at one point: deserializing a `ModelConfig`, where an
    /// all-zero `cost` object is the pre-0.19 encoding of "unknown" and loads
    /// as `None` (see [`ModelConfig::cost`]).
    pub fn is_configured(&self) -> bool {
        self.input_per_million != 0.0
            || self.output_per_million != 0.0
            || self.cache_read_per_million != 0.0
            || self.cache_write_per_million != 0.0
            // A config priced only above its threshold is still priced.
            // Without this, "free below 272K, paid above" reports as unknown
            // and bills every request at $0.
            || self.context_tiers.iter().any(|t| t.is_configured())
    }

    /// Dollar cost of a usage record at these per-million-token rates.
    ///
    /// Consumed by [`crate::Agent::session_cost_usd`]; also usable directly
    /// in `after_turn` callbacks for per-turn cost tracking.
    pub fn cost_usd(&self, usage: &crate::types::Usage) -> f64 {
        // Prompt size, which is what a context tier is priced against — every
        // caller passes one request's usage, so no extra parameter is needed.
        let prompt = usage.input + usage.cache_read + usage.cache_write;
        // The last tier the prompt clears. `with_context_tier` keeps the vec
        // sorted, so this is the most expensive applicable band.
        let tier = self
            .context_tiers
            .iter()
            .rfind(|t| prompt > t.above_prompt_tokens);
        let (input, output, cache_read, cache_write) = match tier {
            Some(t) => (
                t.input_per_million,
                t.output_per_million,
                t.cache_read_per_million,
                t.cache_write_per_million,
            ),
            None => (
                self.input_per_million,
                self.output_per_million,
                self.cache_read_per_million,
                self.cache_write_per_million,
            ),
        };
        (usage.input as f64 * input
            + usage.output as f64 * output
            + usage.cache_read as f64 * cache_read
            + usage.cache_write as f64 * cache_write)
            / 1_000_000.0
    }
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            input_per_million: 0.0,
            output_per_million: 0.0,
            cache_read_per_million: 0.0,
            cache_write_per_million: 0.0,
            context_tiers: Vec::new(),
        }
    }
}

/// How a provider handles the `max_tokens` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

/// How a provider formats thinking/reasoning output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingFormat {
    #[default]
    OpenAi,
    Xai,
    Qwen,
}

/// The highest OpenAI reasoning-effort rung a model accepts.
///
/// OpenAI's effort ladder grew rungs model by model, and a model rejects a
/// value it does not know with a 400 rather than rounding it. The rungs above
/// `high`, per OpenAI's model pages and Azure's reasoning guide ("max works
/// only with GPT-6 or GPT-5.6 models and the Responses API. xhigh works only
/// with GPT-6, GPT-5.6, GPT-5.5, GPT-5.4, and gpt-5.1-codex-max models"):
///
/// | Ceiling | Models (checked 2026-09-25) |
/// |---------|--------|
/// | `High`  | gpt-5, gpt-5.1, o-series, and any model not listed |
/// | `XHigh` | gpt-5.2, gpt-5.4, gpt-5.4-mini, gpt-5.5, gpt-5.1-codex-max |
/// | `Max`   | the GPT-5.6 and GPT-6 families |
///
/// This is a declared capability, never inferred from the model id: a preset
/// sets it, and a caller on a generic constructor sets it for their model.
/// [`ThinkingLevel::XHigh`] sends `xhigh` when the ceiling is at least
/// `XHigh`, else `high`; [`ThinkingLevel::Max`] sends the ceiling itself
/// (`max`, `xhigh` or `high`). Levels at or below `High` are unaffected.
///
/// The default, `High`, is the behaviour before this type existed, and what
/// a persisted [`OpenAiCompat`] without the field deserializes to. Marked
/// `#[non_exhaustive]` so the next rung is not a breaking change.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ReasoningEffortCeiling {
    /// Tops out at `high`. `XHigh` and `Max` are clamped to `high`.
    #[default]
    High,
    /// Accepts `xhigh`. `Max` is sent as `xhigh`.
    XHigh,
    /// Accepts `max` (and `xhigh`).
    Max,
}

/// Compatibility flags for OpenAI-compatible providers.
/// Different providers have different quirks even though they share the same base API.
///
/// Marked `#[non_exhaustive]`: this is the crate's most literal instance of a
/// growing quirk list — every new provider difference adds a flag, and without
/// the attribute each one is a downstream break. Construct from a preset
/// ([`OpenAiCompat::openai`], [`OpenAiCompat::deepseek`], …) or
/// [`Default::default`] and adjust fields. New flags carry `#[serde(default)]`
/// so persisted configs keep deserializing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OpenAiCompat {
    /// Supports the `store` parameter for conversation persistence.
    pub supports_store: bool,
    /// Supports `developer` role (system-level instructions).
    pub supports_developer_role: bool,
    /// Supports `reasoning_effort` parameter.
    pub supports_reasoning_effort: bool,
    /// Supports DeepSeek-style `thinking` mode control.
    #[serde(default)]
    pub supports_thinking_control: bool,
    /// Includes usage data in streaming responses.
    pub supports_usage_in_streaming: bool,
    /// Which field name to use for max tokens.
    pub max_tokens_field: MaxTokensField,
    /// Tool results must include a `name` field.
    pub requires_tool_result_name: bool,
    /// Must insert an assistant message after tool results.
    #[serde(default)]
    pub requires_assistant_after_tool_result: bool,
    /// How thinking/reasoning content is formatted in streaming.
    pub thinking_format: ThinkingFormat,
    /// Accepts OpenAI's `prompt_cache_key` for routing cache lookups.
    ///
    /// Off by default: the field is OpenAI's, and a strict compat server that
    /// validates unknown keys would reject the whole request rather than
    /// ignore it. Providers that cache automatically (DeepSeek, Groq) lose
    /// nothing by leaving this off — they were never reading it.
    #[serde(default)]
    pub supports_prompt_cache_key: bool,
    /// Sends an assistant turn's thinking back as `reasoning_content` when the
    /// request carries tools.
    ///
    /// DeepSeek's thinking mode requires it: "for requests carrying the tools
    /// parameter, the reasoning_content must be fully passed back to the API in
    /// all subsequent requests — even for turns where the model did not perform
    /// a tool call. If your code does not correctly pass back
    /// reasoning_content, the API will return a 400 error"
    /// (<https://api-docs.deepseek.com/guides/thinking_mode>). Without tools
    /// DeepSeek ignores the field, so it is not sent then.
    ///
    /// Off by default: `reasoning_content` is not an OpenAI request field, and
    /// a strict server may reject it. Set by [`OpenAiCompat::deepseek`].
    /// A config persisted before this flag existed deserializes it as `false`;
    /// re-create DeepSeek compat from the preset.
    #[serde(default)]
    pub replays_reasoning_content: bool,
    /// The highest reasoning-effort rung the model accepts; see
    /// [`ReasoningEffortCeiling`]. Defaults to `High`.
    ///
    /// Read by the Chat Completions provider (when
    /// [`supports_reasoning_effort`](Self::supports_reasoning_effort) is set)
    /// **and** by the OpenAI Responses and Azure OpenAI providers, which take
    /// it from `ModelConfig::compat` and ignore every other flag here. Not
    /// read on the DeepSeek ladder
    /// ([`supports_thinking_control`](Self::supports_thinking_control)), which
    /// has its own `low`/`high`/`max` mapping.
    #[serde(default)]
    pub max_reasoning_effort: ReasoningEffortCeiling,
    /// The model accepts reasoning effort `none`, so
    /// [`ThinkingLevel::Off`] is sent as `none` rather than omitted.
    ///
    /// Omitting the effort does not turn reasoning off on OpenAI's reasoning
    /// models — it runs them at their default, `medium`. Where `none` exists
    /// (gpt-5.1 and later, GPT-5.6, GPT-6 Sol/Luna) it is the only way to get
    /// a non-reasoning request, and on GPT-6 Sol/Luna it is also the only
    /// effort under which Chat Completions allows function calling. Leave it
    /// off where `none` is rejected: GPT-6 Astra returns HTTP 400 for it, and
    /// the o-series and gpt-5 do not have it.
    ///
    /// Read by the same providers as
    /// [`max_reasoning_effort`](Self::max_reasoning_effort), with the same
    /// DeepSeek exception (DeepSeek's `Off` is `thinking: disabled`).
    #[serde(default)]
    pub supports_effort_none: bool,
}

impl Default for OpenAiCompat {
    fn default() -> Self {
        Self {
            supports_store: false,
            supports_developer_role: false,
            supports_reasoning_effort: false,
            supports_thinking_control: false,
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxTokens,
            requires_tool_result_name: false,
            requires_assistant_after_tool_result: false,
            thinking_format: ThinkingFormat::OpenAi,
            supports_prompt_cache_key: false,
            replays_reasoning_content: false,
            max_reasoning_effort: ReasoningEffortCeiling::High,
            supports_effort_none: false,
        }
    }
}

impl OpenAiCompat {
    /// OpenAI's reasoning-effort string for `level`, or `None` to omit the
    /// field. Shared by the Chat Completions (`reasoning_effort`), Responses
    /// and Azure (`reasoning.effort`) request builders.
    ///
    /// `Off` is `none` only where [`supports_effort_none`](Self::supports_effort_none)
    /// is set; `XHigh`/`Max` are capped at
    /// [`max_reasoning_effort`](Self::max_reasoning_effort). Not the DeepSeek
    /// ladder — `openai_compat.rs` handles that before reaching here.
    pub(crate) fn openai_reasoning_effort(&self, level: ThinkingLevel) -> Option<&'static str> {
        use ReasoningEffortCeiling as Ceiling;
        Some(match level {
            ThinkingLevel::Off => return self.supports_effort_none.then_some("none"),
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::XHigh if self.max_reasoning_effort >= Ceiling::XHigh => "xhigh",
            ThinkingLevel::XHigh => "high",
            ThinkingLevel::Max => match self.max_reasoning_effort {
                Ceiling::Max => "max",
                Ceiling::XHigh => "xhigh",
                Ceiling::High => "high",
            },
        })
    }

    /// Compat flags for native OpenAI.
    pub fn openai() -> Self {
        Self {
            supports_store: true,
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            supports_prompt_cache_key: true,
            ..Default::default()
        }
    }

    /// Compat flags for the Meta Model API (Muse Spark).
    ///
    /// OpenAI-compatible chat completions. Meta documents `reasoning_effort`
    /// (default `medium` server-side) and streamed usage via
    /// `stream_options.include_usage`; `max_tokens` is deprecated in favor of
    /// `max_completion_tokens`.
    pub fn meta() -> Self {
        Self {
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        }
    }

    /// Compat flags for xAI (Grok).
    ///
    /// xAI documents `reasoning_effort` on Chat Completions ("the supported
    /// values and the default depend on the model";
    /// <https://docs.x.ai/developers/rest-api-reference/inference/chat-completions>).
    /// On grok-4.5/4.6/4.7 it takes `low`/`medium`/`high`, plus `xhigh` on 4.6
    /// and later; it defaults to `high`, and reasoning cannot be disabled
    /// (<https://docs.x.ai/developers/model-capabilities/text/reasoning>). So [`ThinkingLevel::Off`]
    /// sends no `reasoning_effort` and leaves the model at its default `high`
    /// — it does not turn reasoning off.
    ///
    /// The effort ceiling is [`ReasoningEffortCeiling::XHigh`] for every Grok
    /// model, not just 4.6+: xAI documents that on "models that do not
    /// support it, such as grok-4.5, requests with "xhigh" are treated as
    /// "high"" — rounded, not rejected — so `ThinkingLevel::XHigh` and `Max`
    /// send `xhigh`, and older models simply run at `high`.
    ///
    /// [`ThinkingLevel::Off`]: crate::types::ThinkingLevel::Off
    pub fn xai() -> Self {
        Self {
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            thinking_format: ThinkingFormat::Xai,
            max_reasoning_effort: ReasoningEffortCeiling::XHigh,
            ..Default::default()
        }
    }

    /// Compat flags for Groq.
    pub fn groq() -> Self {
        Self {
            supports_usage_in_streaming: true,
            ..Default::default()
        }
    }

    /// Compat flags for Cerebras.
    pub fn cerebras() -> Self {
        Self::default()
    }

    /// Compat flags for OpenRouter.
    pub fn openrouter() -> Self {
        Self {
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        }
    }

    /// Compat flags for Mistral.
    pub fn mistral() -> Self {
        Self {
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxTokens,
            ..Default::default()
        }
    }

    /// Compat flags for DeepSeek.
    pub fn deepseek() -> Self {
        Self {
            supports_reasoning_effort: true,
            supports_thinking_control: true,
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxTokens,
            replays_reasoning_content: true,
            ..Default::default()
        }
    }

    /// Compat flags for Z.ai (Zhipu AI).
    pub fn zai() -> Self {
        Self {
            supports_usage_in_streaming: true,
            ..Default::default()
        }
    }

    /// Compat flags for MiniMax.
    pub fn minimax() -> Self {
        Self {
            supports_usage_in_streaming: true,
            ..Default::default()
        }
    }

    /// Compat flags for Qwen / DashScope.
    pub fn qwen() -> Self {
        Self {
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxTokens,
            thinking_format: ThinkingFormat::Qwen,
            ..Default::default()
        }
    }

    /// Compat flags for Ollama's OpenAI-compatible API.
    pub fn ollama() -> Self {
        Self {
            requires_assistant_after_tool_result: true,
            ..Default::default()
        }
    }
}

/// Quirk flags for the Anthropic Messages protocol (only for AnthropicMessages).
///
/// When `ModelConfig.anthropic` is `None`, providers use `AnthropicCompat::default()`,
/// which targets the current model generation (Claude 4.6+ / Fable 5).
///
/// Marked `#[non_exhaustive]`, like [`OpenAiCompat`]: construct from
/// [`Default::default`] or [`AnthropicCompat::legacy`] and adjust fields.
/// The struct carries `#[serde(default)]`, so persisted configs keep
/// deserializing when a flag is added.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct AnthropicCompat {
    /// Use adaptive thinking (`thinking: {"type": "adaptive"}` plus
    /// `output_config.effort`). Required by Claude Fable 5/5.1, Opus 5.5,
    /// Opus 5, Opus 4.7/4.8, and Sonnet 5; recommended on Opus 4.6 / Sonnet
    /// 4.6. Set to `false` for pre-4.6 models, which only accept
    /// `{"type": "enabled", "budget_tokens": N}`.
    ///
    /// `ThinkingLevel::Off` omits the `thinking` field rather than sending
    /// `{"type": "disabled"}`. That disables thinking only on models that
    /// think on request: Opus 5.5 and Fable 5.1 always think (at their
    /// default effort, `medium` and `high` respectively), and Opus 5 thinks
    /// whenever the field is absent.
    pub adaptive_thinking: bool,
    /// Send the API key as `Authorization: Bearer {key}` instead of the
    /// Anthropic-native `x-api-key` header. Needed for OpenAI-style gateways
    /// that speak the Anthropic Messages protocol (e.g. OpenCode Zen/Go).
    pub bearer_auth: bool,
    /// Enforce [`Agent::prompt_structured`](crate::Agent::prompt_structured)
    /// schemas with the API's native JSON outputs
    /// (`output_config.format = {"type": "json_schema", "schema": ...}`)
    /// instead of forcing a synthetic tool call.
    ///
    /// Required on models that reject forced `tool_choice` (`any` / `tool`)
    /// with a 400 — Claude Fable 5.1 and Opus 5.5 — and supported by Fable 5,
    /// Opus 4.5–5.5, Sonnet 4.5/4.6/5 and Haiku 4.5 on the Claude API. The
    /// Claude presets for those models set it. Unlike tool-forcing it leaves
    /// thinking on and tool choice at `auto`, so regular tools stay callable.
    ///
    /// The API compiles the schema into a grammar and rejects some JSON
    /// Schema features: objects need `"additionalProperties": false`, and
    /// numeric and length constraints (`minimum`, `maxLength`, …) are
    /// unsupported, much as in OpenAI's strict mode.
    ///
    /// Off by default: gateways and older models may not accept
    /// `output_config.format`, while tool-forcing works wherever forced tool
    /// choice does.
    pub native_structured_output: bool,
}

impl Default for AnthropicCompat {
    fn default() -> Self {
        Self {
            adaptive_thinking: true,
            bearer_auth: false,
            native_structured_output: false,
        }
    }
}

impl AnthropicCompat {
    /// Compat flags for pre-4.6 Claude models (budget-based extended thinking).
    pub fn legacy() -> Self {
        Self {
            adaptive_thinking: false,
            ..Self::default()
        }
    }

    /// Set [`native_structured_output`](Self::native_structured_output).
    pub fn with_native_structured_output(mut self, on: bool) -> Self {
        self.native_structured_output = on;
        self
    }

    /// Set [`bearer_auth`](Self::bearer_auth).
    pub fn with_bearer_auth(mut self, on: bool) -> Self {
        self.bearer_auth = on;
        self
    }

    /// Compat flags inferred from a Claude model id, for configs built from
    /// a bare id (the OpenCode gateways) rather than a preset.
    ///
    /// - Thinking: models before 4.6 (Claude 3.x, 4, 4.1, and Sonnet/Opus/
    ///   Haiku 4.5) accept only budget-based extended thinking and reject
    ///   `type: "adaptive"` with a 400, so they get [`legacy`](Self::legacy);
    ///   4.6 and later get adaptive thinking.
    /// - Structured outputs: `output_config.format` is supported from 4.5
    ///   onwards (Haiku 4.5, Sonnet/Opus 4.5+, Fable 5+), so those ids turn on
    ///   [`native_structured_output`](Self::native_structured_output) — needed
    ///   for Fable 5.1 and Opus 5.5, which reject forced tool choice.
    ///
    /// An id with no recognizable version (a new family name, say) is treated
    /// as current generation: adaptive thinking, native structured outputs.
    pub fn for_claude_id(id: &str) -> Self {
        match claude_version(id) {
            Some(v) => {
                let base = if v < (4, 6) {
                    Self::legacy()
                } else {
                    Self::default()
                };
                base.with_native_structured_output(v >= (4, 5))
            }
            None => Self::default().with_native_structured_output(true),
        }
    }
}

/// `(major, minor)` of a Claude model id, in either naming scheme:
/// `claude-{family}-{major}[-{minor}][-{date}]` (`claude-haiku-4-5`,
/// `claude-sonnet-4-20250514`, `claude-opus-4.5`) or the older
/// `claude-{major}[-{minor}]-{family}` (`claude-3-5-sonnet-20241022`).
/// A date suffix is not a minor version. `None` when no version is found.
fn claude_version(id: &str) -> Option<(u32, u32)> {
    let lower = id.to_ascii_lowercase();
    let rest = lower.strip_prefix("claude-")?;
    let mut tokens = rest
        .split(['-', '.'])
        .skip_while(|t| t.parse::<u32>().is_err());
    let major: u32 = tokens.next()?.parse().ok()?;
    let minor = tokens
        .next()
        .filter(|t| t.len() <= 2)
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    Some((major, minor))
}

/// Quirk flags for the Gemini protocols (`GoogleGenerativeAi` and
/// `GoogleVertex`).
///
/// When `ModelConfig.google` is `None`, providers use `GoogleCompat::default()`,
/// which infers everything from the model id.
///
/// Marked `#[non_exhaustive]` so flags can be added without a breaking change:
/// start from [`GoogleCompat::default()`] (or a constructor) and assign fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct GoogleCompat {
    /// Which `thinkingConfig` field carries [`ThinkingLevel`].
    ///
    /// - `None` (default): read it from the model id. Gemini 3 and later get
    ///   `thinkingLevel`; Gemini 2.x, and any id the rule cannot read, get
    ///   `thinkingBudget`. See `docs/providers/google.md` for the rule.
    /// - `Some(true)`: always send `thinkingLevel` — for a proxy alias or
    ///   tuned-model name that hides a Gemini 3 model.
    /// - `Some(false)`: always send `thinkingBudget`, which Gemini 3 still
    ///   accepts for backward compatibility.
    ///
    /// Gemini rejects `thinkingLevel` on models before Gemini 3, and rejects a
    /// request that carries both fields, so exactly one is ever sent.
    ///
    /// [`ThinkingLevel::Off`] sends no `thinkingConfig` whichever field is
    /// chosen. On Gemini 3 that does **not** disable thinking: the model runs
    /// at its own default level (3.1 Pro `HIGH`, 3.5–3.8 Flash `MEDIUM`,
    /// Flash-Lite `MINIMAL`). Use [`ThinkingLevel::Minimal`] to ask for the
    /// least thinking.
    ///
    /// [`ThinkingLevel::Off`]: crate::types::ThinkingLevel::Off
    /// [`ThinkingLevel::Minimal`]: crate::types::ThinkingLevel::Minimal
    pub thinking_level: Option<bool>,
}

impl GoogleCompat {
    /// Always send `thinkingConfig.thinkingLevel`, whatever the model id says.
    pub fn force_thinking_level() -> Self {
        Self {
            thinking_level: Some(true),
        }
    }

    /// Always send `thinkingConfig.thinkingBudget`, whatever the model id says.
    pub fn force_thinking_budget() -> Self {
        Self {
            thinking_level: Some(false),
        }
    }
}

/// The two OpenCode gateways (<https://opencode.ai>).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenCodeGateway {
    /// Pay-per-use gateway (`opencode.ai/zen/v1`).
    Zen,
    /// Subscription gateway for open models (`opencode.ai/zen/go/v1`).
    Go,
}

impl OpenCodeGateway {
    fn provider_name(self) -> &'static str {
        match self {
            Self::Zen => "opencode-zen",
            Self::Go => "opencode-go",
        }
    }

    fn base_url(self) -> &'static str {
        match self {
            Self::Zen => "https://opencode.ai/zen/v1",
            Self::Go => "https://opencode.ai/zen/go/v1",
        }
    }
}

/// Full model configuration. Knows everything needed to make API calls.
///
/// Marked `#[non_exhaustive]`: fields may be added in minor releases (e.g.
/// the `anthropic` compat flags, slated for 0.9.0). Construct via the
/// `ModelConfig::*` preset constructors — or [`ModelConfig::custom`] for
/// protocols without a preset — and mutate fields to customize. Note that
/// downstream struct literals and functional-record-update
/// (`ModelConfig { .. }`) no longer compile; field mutation is the supported
/// pattern. New fields must carry `#[serde(default)]` so previously
/// persisted configs keep deserializing.
#[derive(Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelConfig {
    /// Model identifier sent to the API (e.g. "gpt-4o", "claude-sonnet-4-20250514").
    pub id: String,
    /// Human-friendly name.
    pub name: String,
    /// Which API protocol to use.
    pub api: ApiProtocol,
    /// Provider name (e.g. "openai", "anthropic", "xai").
    pub provider: String,
    /// Base URL for API requests (without trailing slash).
    pub base_url: String,
    /// Whether this model supports reasoning/thinking. When `false` and a
    /// `thinking_level` is requested, the [`Agent`](crate::Agent) wrapper
    /// logs a warning; sub-agents and direct `agent_loop` calls do not. The
    /// request is still sent either way — gate behavior stays with the
    /// caller.
    pub reasoning: bool,
    /// Context window size in tokens.
    pub context_window: u32,
    /// Default max output tokens.
    pub max_tokens: u32,
    /// Per-token rates, or `None` when this crate does not know the price.
    ///
    /// `None` means **unknown**, not free: the generic constructors
    /// ([`openai`](Self::openai), [`deepseek`](Self::deepseek),
    /// [`anthropic`](Self::anthropic), [`custom`](Self::custom), …) take any
    /// model id, so they cannot carry a price. Only the named presets whose
    /// rates were checked against the vendor's page (`claude_fable_5_1`,
    /// `gpt_5_5`, …) return `Some`. Before 0.19 this field was a bare
    /// `CostConfig` and unpriced configs held all-zero rates, which read as a
    /// $0 model to anyone who did not know to call `is_configured()`.
    ///
    /// `Some` with every rate zero means **free**: a local model, or a free
    /// tier. The built-in accounting
    /// ([`Agent::session_cost_usd`](crate::Agent::session_cost_usd), the
    /// `llm_stream` span's `cost_usd`, `SessionStats::cost_usd`) reports
    /// `Some(0.0)` for it and `None` only for `cost: None`.
    ///
    /// Serde: a missing or `null` `cost` is `None`, and `None` is omitted on
    /// serialize, so older releases still read the output. A `cost` object
    /// with **every rate zero** (tiers included — [`CostConfig::is_configured`]
    /// false) also deserializes to `None`, because that is how every release
    /// before 0.19 persisted "unknown", and reading those as free would report
    /// $0 for models that bill. The trade-off: a free config written by this
    /// release comes back from disk as `None` (unknown), not free. Reapply
    /// `Some(CostConfig::new(0.0, 0.0))` after loading if you persist one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_cost"
    )]
    pub cost: Option<CostConfig>,
    /// Additional headers to send with requests.
    ///
    /// May carry credentials (`Authorization`, `x-api-key`). `Debug` prints
    /// header *names* with redacted values, but `Serialize` is intentionally
    /// lossless so configs round-trip — do not serialize a `ModelConfig` into
    /// logs or telemetry.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// OpenAI-compat quirk flags (only for OpenAiCompletions protocol).
    #[serde(default)]
    pub compat: Option<OpenAiCompat>,
    /// Anthropic Messages quirk flags (only for AnthropicMessages protocol).
    /// `None` behaves like `AnthropicCompat::default()` (current generation).
    #[serde(default)]
    pub anthropic: Option<AnthropicCompat>,
    /// Gemini quirk flags (only for the `GoogleGenerativeAi` and
    /// `GoogleVertex` protocols). `None` behaves like
    /// `GoogleCompat::default()`: everything is inferred from the model id.
    ///
    /// Omitted on serialize when `None`, so configs that never set it
    /// serialize exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google: Option<GoogleCompat>,
}

/// `ModelConfig::cost`'s deserializer: an all-zero object is the pre-0.19
/// encoding of "unknown", so it loads as `None` rather than as free.
fn deserialize_cost<'de, D>(deserializer: D) -> Result<Option<CostConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cost = Option::<CostConfig>::deserialize(deserializer)?;
    Ok(cost.filter(CostConfig::is_configured))
}

/// Redacts header values. Headers routinely carry credentials, and a
/// derived `Debug` would print them into any log line or panic message.
impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<&str> = self.headers.keys().map(String::as_str).collect();
        f.debug_struct("ModelConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("api", &self.api)
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("reasoning", &self.reasoning)
            .field("context_window", &self.context_window)
            .field("max_tokens", &self.max_tokens)
            .field("header_names", &headers)
            .finish_non_exhaustive()
    }
}

impl ModelConfig {
    /// A minimal config for tests. `provider` is `"mock"`, `cost` is `None`
    /// (unpriced), and `base_url` points at a non-routable host.
    ///
    /// Use it **only** with
    /// [`Agent::from_provider`](crate::Agent::from_provider) /
    /// [`SubAgentTool::from_provider`](crate::SubAgentTool::from_provider) and a
    /// [`MockProvider`](crate::provider::MockProvider): those take the provider
    /// explicitly, so the config's protocol is never consulted.
    ///
    /// **Do not** pass it to
    /// [`Agent::from_config`](crate::Agent::from_config) — that dispatches on
    /// the protocol (here `AnthropicMessages`) and would build the **real**
    /// Anthropic provider pointed at the non-routable `base_url`, so the first
    /// prompt fails with a network error instead of returning a mock response.
    pub fn mock() -> Self {
        Self::custom(
            ApiProtocol::AnthropicMessages,
            "mock",
            "http://mock.invalid",
            "mock",
            "Mock",
        )
    }

    /// Create a config for any protocol without a dedicated preset
    /// (Bedrock, Vertex, Azure, or future protocols).
    ///
    /// Since `ModelConfig` is `#[non_exhaustive]`, this is the construction
    /// path when no `ModelConfig::*` preset fits. Defaults: 128K context,
    /// 16K max output, no compat flags — mutate fields to adjust.
    pub fn custom(
        api: ApiProtocol,
        provider: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            id: model_id.into(),
            name: name.into(),
            api,
            provider: provider.into(),
            base_url: base_url.into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 16_000,
            cost: None,
            headers: HashMap::new(),
            google: None,
            compat: None,
            anthropic: None,
        }
    }

    /// Create a new Anthropic model config.
    pub fn anthropic(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::AnthropicMessages,
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            reasoning: true,
            context_window: 200_000,
            max_tokens: 16_000,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: None,
        }
    }

    /// Claude Fable 5 — Anthropic's most capable model.
    /// 1M context; defaults to 64K of the model's 128K max output.
    ///
    /// Rates verified against <https://platform.claude.com/docs/en/about-claude/pricing>
    /// on 2026-08-19. See [`CostConfig`] — they are a snapshot, not an authority.
    pub fn claude_fable_5() -> Self {
        Self {
            context_window: 1_000_000,
            max_tokens: 64_000,
            cost: Some(
                CostConfig::new(10.0, 50.0)
                    .with_cache_read(1.0)
                    .with_cache_write(12.5),
            ),
            anthropic: Some(AnthropicCompat::default().with_native_structured_output(true)),
            ..Self::anthropic("claude-fable-5", "Claude Fable 5")
        }
    }

    /// Claude Fable 5.1. 1M context; defaults to 64K of the model's 128K max
    /// output.
    ///
    /// Priced like [`claude_fable_5`](Self::claude_fable_5) in every column but
    /// one: cache hits bill at **0.025x** input ($0.25/MTok) rather than 0.1x
    /// ($1.00). On a cache-heavy agent loop that line dominates the bill, so a
    /// consumer that maps `claude-fable-5-1` onto `claude_fable_5()` by prefix
    /// reports cache reads 4x high. `cache_write_per_million` is the 5-minute
    /// write rate ($12.50); the 1-hour rate ($20) is not modelled because this
    /// crate only places 5-minute (`ephemeral`) breakpoints.
    ///
    /// **Not a drop-in for Fable 5 at the API level:**
    /// - Forced `tool_choice` (`any` / `tool`) is rejected with a 400. The
    ///   preset sets [`AnthropicCompat::native_structured_output`], so
    ///   [`Agent::prompt_structured`](crate::Agent::prompt_structured) uses
    ///   `output_config.format` and never forces a tool. A hand-built
    ///   `ModelConfig::anthropic("claude-fable-5-1", ..)` without that flag
    ///   still forces one and gets the 400.
    /// - Thinking is always on (adaptive); `ThinkingLevel::Off` omits the
    ///   field rather than sending `disabled`, which this model rejects, and
    ///   the model still thinks at its default effort (`high`).
    /// - Thinking blocks are bound to the model that produced them, and editing
    ///   earlier turns invalidates them — switching a session to or from this
    ///   model with [`Agent::set_model`](crate::Agent::set_model) carries
    ///   blocks the other model cannot read.
    ///
    /// Rates verified against the raw markup of
    /// <https://platform.claude.com/docs/en/about-claude/pricing> on
    /// 2026-09-24. See [`CostConfig`] — they are a snapshot, not an authority.
    pub fn claude_fable_5_1() -> Self {
        Self {
            context_window: 1_000_000,
            max_tokens: 64_000,
            cost: Some(
                CostConfig::new(10.0, 50.0)
                    .with_cache_read(0.25)
                    .with_cache_write(12.5),
            ),
            anthropic: Some(AnthropicCompat::default().with_native_structured_output(true)),
            ..Self::anthropic("claude-fable-5-1", "Claude Fable 5.1")
        }
    }

    /// Claude Opus 5.5. 1M context; defaults to 64K of the model's 128K max
    /// output.
    ///
    /// Cache hits bill at **0.05x** input ($0.20/MTok), not the usual 0.1x.
    /// `cache_write_per_million` is the 5-minute write rate ($5); the 1-hour
    /// rate ($8) is not modelled because this crate only places 5-minute
    /// (`ephemeral`) breakpoints.
    ///
    /// **Not a drop-in for Opus 5 at the API level:**
    /// - Thinking is always on (adaptive) and cannot be disabled.
    ///   `ThinkingLevel::Off` omits the `thinking` field (the API rejects
    ///   `disabled` and budget-based thinking), so the model still thinks at
    ///   its default effort, `medium` — thinking tokens count against
    ///   `max_tokens` and bill as output. Other levels send `output_config.effort`
    ///   (`low` through `max` are all accepted).
    /// - Forced `tool_choice` (`any` / `tool`) is rejected with a 400. The
    ///   preset sets [`AnthropicCompat::native_structured_output`], so
    ///   [`Agent::prompt_structured`](crate::Agent::prompt_structured) uses
    ///   `output_config.format` and never forces a tool.
    /// - `temperature` (and `top_p` / `top_k`) other than the default is
    ///   rejected with a 400: leave `StreamConfig::temperature` unset.
    /// - Thinking blocks are bound to the model and the conversation prefix:
    ///   editing earlier turns invalidates them, and switching a session to
    ///   or from this model with [`Agent::set_model`](crate::Agent::set_model)
    ///   carries blocks the other model cannot read (Opus 5.5 reads Opus 5
    ///   and earlier Opus/Sonnet/Haiku thinking, but not Fable's).
    ///
    /// Rates verified against the raw markup of
    /// <https://platform.claude.com/docs/en/about-claude/pricing> on
    /// 2026-09-25. See [`CostConfig`] — they are a snapshot, not an authority.
    pub fn claude_opus_5_5() -> Self {
        Self {
            context_window: 1_000_000,
            max_tokens: 64_000,
            cost: Some(
                CostConfig::new(4.0, 20.0)
                    .with_cache_read(0.2)
                    .with_cache_write(5.0),
            ),
            anthropic: Some(AnthropicCompat::default().with_native_structured_output(true)),
            ..Self::anthropic("claude-opus-5-5", "Claude Opus 5.5")
        }
    }

    /// Claude Opus 5. 1M context; defaults to 64K of the model's 128K max output.
    ///
    /// Opus 5 thinks whenever a request omits `thinking`, so `ThinkingLevel::Off`
    /// does not disable thinking here — the provider omits the field rather than
    /// sending `{"type": "disabled"}`, and those tokens still count against
    /// `max_tokens`. Any other level takes the adaptive path that
    /// `AnthropicCompat::default()` selects, which Opus 5 accepts unchanged.
    ///
    /// Rates verified against <https://platform.claude.com/docs/en/about-claude/pricing>
    /// on 2026-08-19. See [`CostConfig`] — they are a snapshot, not an authority.
    pub fn claude_opus_5() -> Self {
        Self {
            context_window: 1_000_000,
            max_tokens: 64_000,
            cost: Some(
                CostConfig::new(5.0, 25.0)
                    .with_cache_read(0.5)
                    .with_cache_write(6.25),
            ),
            anthropic: Some(AnthropicCompat::default().with_native_structured_output(true)),
            ..Self::anthropic("claude-opus-5", "Claude Opus 5")
        }
    }

    /// Claude Opus 4.8. 1M context; defaults to 64K of the model's 128K max output.
    ///
    /// Rates verified against <https://platform.claude.com/docs/en/about-claude/pricing>
    /// on 2026-08-19. See [`CostConfig`] — they are a snapshot, not an authority.
    pub fn claude_opus_4_8() -> Self {
        Self {
            context_window: 1_000_000,
            max_tokens: 64_000,
            cost: Some(
                CostConfig::new(5.0, 25.0)
                    .with_cache_read(0.5)
                    .with_cache_write(6.25),
            ),
            anthropic: Some(AnthropicCompat::default().with_native_structured_output(true)),
            ..Self::anthropic("claude-opus-4-8", "Claude Opus 4.8")
        }
    }

    /// Claude Sonnet 5. 1M context; defaults to 64K of the model's 128K max output.
    ///
    /// Rates verified against <https://platform.claude.com/docs/en/about-claude/pricing>
    /// on 2026-08-19. See [`CostConfig`] — they are a snapshot, not an authority.
    pub fn claude_sonnet_5() -> Self {
        Self {
            context_window: 1_000_000,
            max_tokens: 64_000,
            cost: Some(
                CostConfig::new(2.0, 10.0)
                    .with_cache_read(0.2)
                    .with_cache_write(2.5),
            ),
            anthropic: Some(AnthropicCompat::default().with_native_structured_output(true)),
            ..Self::anthropic("claude-sonnet-5", "Claude Sonnet 5")
        }
    }

    /// Claude Haiku 4.5. 200K context; defaults to 32K of the model's 64K max output.
    ///
    /// Uses budget-based extended thinking ([`AnthropicCompat::legacy`]):
    /// Haiku 4.5 does not support adaptive thinking.
    ///
    /// Rates verified against <https://platform.claude.com/docs/en/about-claude/pricing>
    /// on 2026-08-19. See [`CostConfig`] — they are a snapshot, not an authority.
    pub fn claude_haiku_4_5() -> Self {
        Self {
            context_window: 200_000,
            max_tokens: 32_000,
            cost: Some(
                CostConfig::new(1.0, 5.0)
                    .with_cache_read(0.1)
                    .with_cache_write(1.25),
            ),
            // Haiku 4.5 predates adaptive thinking: it accepts only
            // `{"type": "enabled", "budget_tokens": N}` and rejects
            // `{"type": "adaptive"}` with a 400.
            anthropic: Some(AnthropicCompat::legacy().with_native_structured_output(true)),
            ..Self::anthropic("claude-haiku-4-5", "Claude Haiku 4.5")
        }
    }

    /// GPT-5.5. ~1M context; defaults to 64K of the model's 128K max output.
    /// Uses the Chat Completions API.
    ///
    /// Reasoning effort: `none`/`low`/`medium` (default)/`high`/`xhigh`, per
    /// the model page — so the ceiling is [`ReasoningEffortCeiling::XHigh`]
    /// (`ThinkingLevel::Max` sends `xhigh`) and `ThinkingLevel::Off` sends
    /// `none` ([`OpenAiCompat::supports_effort_none`]).
    ///
    /// **Priced in two bands.** The model page
    /// (<https://developers.openai.com/api/docs/models/gpt-5.5>, read
    /// 2026-09-25) lists $5 input / $0.50 cached input / $30 output and states:
    /// "For GPT-5.5, prompts with >272K input tokens are priced at 2x input and
    /// 1.5x output for the full session". So above 272K prompt tokens this
    /// preset bills $10 input / $45 output (a [`ContextTier`] at 272,000;
    /// exclusive, so a prompt of exactly 272,000 stays on the base band).
    /// OpenAI charges nothing extra for cache writes on this model.
    ///
    /// **The long-band cached-input rate, $1.00, is UNVERIFIED.** The sentence
    /// above names input and output only. $1.00 (2x, as the GPT-6 pages state
    /// for their cache rates) is what models.dev records; the literal reading
    /// would leave it at $0.50. It is set rather than left unset because an
    /// unset tier rate is $0, not "inherit the base rate" — leaving it out
    /// would bill every cached token above 272K as free. OpenAI's pricing
    /// page no longer lists gpt-5.5, so there is no table cell to settle it.
    ///
    /// Rates are a snapshot, not an authority; see [`CostConfig`].
    pub fn gpt_5_5() -> Self {
        Self {
            reasoning: true,
            context_window: 1_000_000,
            max_tokens: 64_000,
            cost: Some(
                CostConfig::new(5.0, 30.0)
                    .with_cache_read(0.5)
                    .with_context_tier(ContextTier::new(272_000, 10.0, 45.0).with_cache_read(1.0)),
            ),
            compat: Some(OpenAiCompat {
                max_reasoning_effort: ReasoningEffortCeiling::XHigh,
                supports_effort_none: true,
                ..OpenAiCompat::openai()
            }),
            ..Self::openai("gpt-5.5", "GPT-5.5")
        }
    }

    /// GPT-6 Astra (`gpt-6-astra`), OpenAI's most capable GPT-6 model.
    /// 1,050,000-token context; defaults to 64K of the model's 128K max output.
    ///
    /// **Uses the Responses API**
    /// ([`openai_responses`](Self::openai_responses)). OpenAI: "Chat
    /// Completions does not support function calling with GPT-6 Astra" — an
    /// agent with tools needs Responses.
    ///
    /// Reasoning effort: `low`/`medium`/`high`/`xhigh`/`max`, ceiling
    /// [`ReasoningEffortCeiling::Max`]. **No `none`**: "Setting
    /// reasoning.effort … to none returns HTTP 400", so
    /// [`OpenAiCompat::supports_effort_none`] is off and `ThinkingLevel::Off`
    /// omits the effort — which runs the model at its default, not without
    /// reasoning. `Minimal` is sent as `low` (GPT-6 has no `minimal`).
    ///
    /// **`temperature` is rejected** while reasoning effort is anything but
    /// `none` ("remove temperature, top_p, and top_logprobs"), and Astra
    /// cannot run at `none` — do not set a temperature with this preset.
    ///
    /// Priced per OpenAI's pricing page (read 2026-09-25): $10 input / $1
    /// cached / $12.50 cache write / $50 output; above 272K prompt tokens the
    /// whole request bills at $20 / $2 / $25 / $75. See [`CostConfig`].
    pub fn gpt_6_astra() -> Self {
        Self::gpt_6(
            "gpt-6-astra",
            "GPT-6 Astra",
            false,
            CostConfig::new(10.0, 50.0)
                .with_cache_read(1.0)
                .with_cache_write(12.5)
                .with_context_tier(
                    ContextTier::new(272_000, 20.0, 75.0)
                        .with_cache_read(2.0)
                        .with_cache_write(25.0),
                ),
        )
    }

    /// GPT-6 Sol (`gpt-6-sol`), built for coding and agentic work.
    /// 1,050,000-token context; defaults to 64K of the model's 128K max output.
    ///
    /// **Uses the Responses API**
    /// ([`openai_responses`](Self::openai_responses)). On Chat Completions,
    /// OpenAI allows function calling "only with reasoning_effort set to
    /// none", which would forbid reasoning in any agent with tools.
    ///
    /// Reasoning effort: `none`/`low`/`medium` (default)/`high`/`xhigh`/`max`,
    /// ceiling [`ReasoningEffortCeiling::Max`];
    /// [`OpenAiCompat::supports_effort_none`] is on, so `ThinkingLevel::Off`
    /// sends `none` — no reasoning — rather than falling back to `medium`.
    ///
    /// **`temperature` is rejected** unless the effort is `none`
    /// (`ThinkingLevel::Off`): "When reasoning effort is not none, remove
    /// temperature, top_p, and top_logprobs."
    ///
    /// Priced per OpenAI's pricing page (read 2026-09-25): $2 input / $0.20
    /// cached / $2.50 cache write / $10 output; above 272K prompt tokens the
    /// whole request bills at $4 / $0.40 / $5 / $15. See [`CostConfig`].
    pub fn gpt_6_sol() -> Self {
        Self::gpt_6(
            "gpt-6-sol",
            "GPT-6 Sol",
            true,
            CostConfig::new(2.0, 10.0)
                .with_cache_read(0.2)
                .with_cache_write(2.5)
                .with_context_tier(
                    ContextTier::new(272_000, 4.0, 15.0)
                        .with_cache_read(0.4)
                        .with_cache_write(5.0),
                ),
        )
    }

    /// GPT-6 Luna (`gpt-6-luna`), the efficient GPT-6 model for focused,
    /// high-volume tasks. 1,050,000-token context; defaults to 64K of the
    /// model's 128K max output.
    ///
    /// **Uses the Responses API**, for the same reason as
    /// [`gpt_6_sol`](Self::gpt_6_sol): Chat Completions allows function
    /// calling only at reasoning effort `none`.
    ///
    /// Reasoning effort: `none`/`low`/`medium` (default)/`high`/`xhigh`/`max`,
    /// ceiling [`ReasoningEffortCeiling::Max`]; `ThinkingLevel::Off` sends
    /// `none`. **`temperature` is rejected** unless the effort is `none`.
    ///
    /// Priced per OpenAI's pricing page (read 2026-09-25): $0.10 input /
    /// $0.01 cached / $0.125 cache write / $0.50 output; above 272K prompt
    /// tokens the whole request bills at $0.20 / $0.02 / $0.25 / $0.75. See
    /// [`CostConfig`].
    pub fn gpt_6_luna() -> Self {
        Self::gpt_6(
            "gpt-6-luna",
            "GPT-6 Luna",
            true,
            CostConfig::new(0.1, 0.5)
                .with_cache_read(0.01)
                .with_cache_write(0.125)
                .with_context_tier(
                    ContextTier::new(272_000, 0.2, 0.75)
                        .with_cache_read(0.02)
                        .with_cache_write(0.25),
                ),
        )
    }

    /// Shared shape of the GPT-6 presets. There is no bare `gpt-6` model id.
    ///
    /// `compat` carries only the effort capability here — the Responses
    /// provider reads nothing else from it. It starts from
    /// [`OpenAiCompat::openai`] so that switching `api` to Chat Completions
    /// yields correct flags rather than the bare defaults.
    fn gpt_6(id: &str, name: &str, supports_effort_none: bool, cost: CostConfig) -> Self {
        Self {
            reasoning: true,
            context_window: 1_050_000,
            max_tokens: 64_000,
            cost: Some(cost),
            compat: Some(OpenAiCompat {
                max_reasoning_effort: ReasoningEffortCeiling::Max,
                supports_effort_none,
                ..OpenAiCompat::openai()
            }),
            ..Self::openai_responses(id, name)
        }
    }

    /// Create a new OpenAI model config.
    pub fn openai(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::openai()),
        }
    }

    /// Create a config for OpenAI's **Responses API** (`POST /v1/responses`).
    ///
    /// [`openai`](Self::openai) targets Chat Completions; this targets the
    /// Responses API, OpenAI's native interface for reasoning models (it
    /// streams reasoning summaries and reports cache writes). With
    /// [`Agent::from_config`](crate::Agent::from_config) it resolves to
    /// [`OpenAiResponsesProvider`](crate::provider::OpenAiResponsesProvider)
    /// and reads the key from `OPENAI_API_KEY`.
    ///
    /// Unpriced (`cost: None`); set `cost` for the model you use.
    ///
    /// `compat` is `None`, so reasoning effort tops out at `high` and
    /// `ThinkingLevel::Off` omits it. For a model with a higher ceiling or a
    /// `none` rung, set `compat` to an [`OpenAiCompat`] carrying
    /// [`max_reasoning_effort`](OpenAiCompat::max_reasoning_effort) /
    /// [`supports_effort_none`](OpenAiCompat::supports_effort_none) — the
    /// Responses provider reads those two fields and ignores the rest.
    pub fn openai_responses(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiResponses,
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            context_window: 128_000,
            max_tokens: 16_000,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: None,
        }
    }

    /// Create a config for a local OpenAI-compatible server (LM Studio, Ollama, etc.).
    /// No API key required — sends an empty Bearer token.
    pub fn local(base_url: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            id: model_id.into(),
            name: "Local Model".into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "local".into(),
            base_url: base_url.into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::default()),
        }
    }

    /// Create a config for a model served by OpenCode Zen
    /// (<https://opencode.ai/docs/zen>), OpenCode's pay-per-use gateway.
    ///
    /// Zen serves each model family over a different protocol; the protocol is
    /// selected from the model id:
    /// - `gpt-*`, `grok-*`, `muse-spark-*` → OpenAI Responses API
    ///   (pair with `OpenAiResponsesProvider`)
    /// - `claude-*`, `qwen*` → Anthropic Messages API (pair with `AnthropicProvider`)
    /// - everything else (DeepSeek, MiniMax, GLM, Kimi, ...) → Chat Completions
    ///   (pair with `OpenAiCompatProvider`)
    ///
    /// Gemini models are not supported — Zen serves them over a Google-native
    /// endpoint shape yoagent does not target. A `gemini-*` id falls through to
    /// Chat Completions (with a warning) and will likely fail at request time.
    /// Jev models (`jev-*`) are not supported either: Zen serves them on its
    /// `/systemone` evaluation endpoint. They fall through the same way, with
    /// a warning.
    ///
    /// `claude-*` ids get [`AnthropicCompat::for_claude_id`]: budget thinking
    /// before Claude 4.6, adaptive from 4.6, and native structured outputs
    /// from 4.5 (so `prompt_structured` works on Fable 5.1 and Opus 5.5).
    ///
    /// The routing mirrors the Zen endpoint table as of September 2026; if a model
    /// errors, verify its protocol against `https://opencode.ai/zen/v1/models`.
    ///
    /// Context window and max output default conservatively (128K / 16K);
    /// override the fields for models with larger limits.
    pub fn opencode_zen(model_id: impl Into<String>) -> Self {
        Self::opencode(model_id.into(), OpenCodeGateway::Zen)
    }

    /// Create a config for a model served by OpenCode Go
    /// (<https://opencode.ai/docs/go>), OpenCode's subscription gateway for
    /// open models.
    ///
    /// Protocol is selected from the model id:
    /// - `gpt-*`, `grok-*`, `muse-spark-*` → OpenAI Responses API
    ///   (pair with `OpenAiResponsesProvider`)
    /// - `qwen*`, `minimax-*` → Anthropic Messages API (pair with `AnthropicProvider`)
    /// - everything else (GLM, Kimi, DeepSeek, MiMo, ...) → Chat Completions
    ///   (pair with `OpenAiCompatProvider`)
    ///
    /// The routing mirrors the Go endpoint table as of September 2026.
    pub fn opencode_go(model_id: impl Into<String>) -> Self {
        Self::opencode(model_id.into(), OpenCodeGateway::Go)
    }

    fn opencode(id: String, gateway: OpenCodeGateway) -> Self {
        let lower = id.to_ascii_lowercase();
        if lower.starts_with("gemini-") {
            tracing::warn!(
                "OpenCode serves Gemini models over a Google-native endpoint yoagent \
                 does not target; '{}' is routed to /chat/completions and will likely \
                 fail at request time",
                id
            );
        } else if lower.starts_with("jev-") {
            tracing::warn!(
                "OpenCode serves Jev models on its /systemone evaluation endpoint, \
                 which yoagent does not target; '{}' is routed to /chat/completions \
                 and will likely fail at request time",
                id
            );
        }
        let anthropic_protocol = match gateway {
            OpenCodeGateway::Zen => lower.starts_with("claude-") || lower.starts_with("qwen"),
            OpenCodeGateway::Go => lower.starts_with("qwen") || lower.starts_with("minimax-"),
        };
        let (api, reasoning, compat, anthropic) = if anthropic_protocol {
            (
                ApiProtocol::AnthropicMessages,
                true,
                None,
                // Gateways use OpenAI-style Bearer auth, not x-api-key.
                // Claude ids carry their generation's thinking mode and
                // structured-output support; other families (Qwen, MiniMax)
                // keep the defaults.
                Some(
                    if lower.starts_with("claude-") {
                        AnthropicCompat::for_claude_id(&lower)
                    } else {
                        AnthropicCompat::default()
                    }
                    .with_bearer_auth(true),
                ),
            )
        } else if lower.starts_with("gpt-")
            || lower.starts_with("grok-")
            || lower.starts_with("muse-spark-")
        {
            // Both gateways serve these families on `/responses` only.
            (ApiProtocol::OpenAiResponses, true, None, None)
        } else {
            (
                ApiProtocol::OpenAiCompletions,
                false,
                Some(OpenAiCompat::default()),
                None,
            )
        };
        Self {
            id: id.clone(),
            name: id,
            api,
            provider: gateway.provider_name().into(),
            base_url: gateway.base_url().into(),
            reasoning,
            context_window: 128_000,
            max_tokens: 16_000,
            cost: None,
            headers: HashMap::new(),
            google: None,
            compat,
            anthropic,
        }
    }

    /// Create a config for a custom OpenAI-compatible endpoint with explicit compat flags.
    pub fn openai_compat(
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        provider: impl Into<String>,
        compat: OpenAiCompat,
    ) -> Self {
        let id = model_id.into();
        Self {
            id: id.clone(),
            name: id,
            api: ApiProtocol::OpenAiCompletions,
            provider: provider.into(),
            base_url: base_url.into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(compat),
        }
    }

    /// Create a config for Ollama's OpenAI-compatible API.
    ///
    /// Default local base URL: `http://localhost:11434/v1`.
    pub fn ollama(base_url: impl Into<String>, model_id: impl Into<String>) -> Self {
        let id = model_id.into();
        Self {
            id: id.clone(),
            name: id,
            api: ApiProtocol::OpenAiCompletions,
            provider: "ollama".into(),
            base_url: base_url.into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::ollama()),
        }
    }

    /// Create a new Z.ai (Zhipu AI) model config.
    ///
    /// Models: `glm-4.7`, `glm-4.5-air`, `glm-5`, etc.
    pub fn zai(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "zai".into(),
            base_url: "https://api.z.ai/api/paas/v4".into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::zai()),
        }
    }

    /// Create a new Meta Model API config (Muse Spark).
    ///
    /// Models: `muse-spark-1.1` — 1,048,576-token context; 128K max output
    /// per Meta's integration examples (no official model card yet).
    /// US-only public preview as of July 2026. OpenAI-compatible endpoint at
    /// `https://api.meta.ai/v1`. Key resolves from `META_API_KEY`, then
    /// Meta's documented `MODEL_API_KEY`.
    ///
    /// Reasoning: Meta's endpoint defaults to `reasoning_effort: medium`
    /// server-side. Set a [`ThinkingLevel`] to
    /// tune it; `Off` omits the field, which means Meta's default (medium)
    /// applies — not "no reasoning".
    ///
    /// Rates are Muse Spark 1.1/1.2, verified 2026-08-19. This constructor is
    /// generic over the model id, so a different tier needs `config.cost`
    /// overridden: the contributor tier runs 12x lower on input, 21x on output
    /// and 75x on cache reads, so `ModelConfig::meta("muse-spark-1.2-contributor", ..)`
    /// overstates cost badly. See [`CostConfig`].
    pub fn meta(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "meta".into(),
            base_url: "https://api.meta.ai/v1".into(),
            reasoning: true,
            context_window: 1_048_576,
            max_tokens: 131_072,
            cost: Some(CostConfig::new(1.25, 4.25).with_cache_read(0.15)),
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::meta()),
        }
    }

    /// Create a new MiniMax model config.
    ///
    /// Models: `MiniMax-M3`, `MiniMax-M2.7`, etc. Served from
    /// `https://api.minimax.io/v1`.
    pub fn minimax(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "minimax".into(),
            base_url: "https://api.minimax.io/v1".into(),
            reasoning: false,
            context_window: 1_000_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::minimax()),
        }
    }

    /// Create a new Qwen / DashScope model config.
    ///
    /// Models: `qwen3.6-plus`, `qwen3.5-plus`, `qwen-plus`, `qwen-flash`, etc.
    pub fn qwen(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "qwen".into(),
            base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1".into(),
            reasoning: true,
            context_window: 128_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::qwen()),
        }
    }

    /// Create a new xAI (Grok) model config.
    ///
    /// Models: `grok-4.7`, `grok-4.6`, etc.
    pub fn xai(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "xai".into(),
            base_url: "https://api.x.ai/v1".into(),
            reasoning: false,
            context_window: 131_072,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::xai()),
        }
    }

    /// Create a new Groq model config.
    ///
    /// Models: `openai/gpt-oss-120b`, `openai/gpt-oss-20b`, etc.
    pub fn groq(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "groq".into(),
            base_url: "https://api.groq.com/openai/v1".into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::groq()),
        }
    }

    /// Create a new DeepSeek model config.
    ///
    /// Models: `deepseek-flash` (V4.1 Flash), `deepseek-v4-pro`.
    ///
    /// The legacy names `deepseek-chat` and `deepseek-reasoner` were
    /// discontinued on 2026-07-24. `deepseek-v4-flash` is still accepted but
    /// served by V4.1 Flash; prefer `deepseek-flash`.
    pub fn deepseek(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            reasoning: true,
            context_window: 1_000_000,
            max_tokens: 384_000,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::deepseek()),
        }
    }

    /// Create a new Mistral model config.
    ///
    /// Models: `mistral-large-latest`, `mistral-small-latest`, etc.
    pub fn mistral(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "mistral".into(),
            base_url: "https://api.mistral.ai/v1".into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 4096,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: Some(OpenAiCompat::mistral()),
        }
    }

    /// Create a new Google Generative AI (Gemini) model config.
    pub fn google(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::GoogleGenerativeAi,
            provider: "google".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            reasoning: false,
            context_window: 1_000_000,
            max_tokens: 8192,
            cost: None,
            headers: HashMap::new(),
            google: None,
            anthropic: None,
            compat: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_preset_matches_launch_specs() {
        let mc = ModelConfig::meta("muse-spark-1.1", "Muse Spark 1.1");
        assert_eq!(mc.provider, "meta");
        assert_eq!(mc.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(mc.base_url, "https://api.meta.ai/v1");
        assert_eq!(mc.context_window, 1_048_576);
        assert_eq!(mc.max_tokens, 131_072);
        assert!(mc.cost.as_ref().unwrap().is_configured());
        assert_eq!(mc.cost.as_ref().unwrap().input_per_million, 1.25);
        assert_eq!(mc.cost.as_ref().unwrap().output_per_million, 4.25);
        // Meta documents a cached-input rate; cache writes are not charged.
        assert_eq!(mc.cost.as_ref().unwrap().cache_read_per_million, 0.15);
        assert_eq!(mc.cost.as_ref().unwrap().cache_write_per_million, 0.0);
        let compat = mc.compat.expect("compat flags set");
        assert!(matches!(
            compat.max_tokens_field,
            MaxTokensField::MaxCompletionTokens
        ));
        // Documented in Meta's chat-completions schemas.
        assert!(compat.supports_reasoning_effort);
        assert!(compat.supports_usage_in_streaming);
    }

    #[test]
    fn test_model_config_anthropic() {
        let config = ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5");
        assert_eq!(config.api, ApiProtocol::AnthropicMessages);
        assert_eq!(config.provider, "anthropic");
        assert_eq!(config.base_url, "https://api.anthropic.com/v1");
        assert!(config.compat.is_none());
        assert!(config.anthropic.is_none());
    }

    #[test]
    fn test_cost_usd() {
        let cost = CostConfig {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_read_per_million: 0.3,
            cache_write_per_million: 3.75,
            ..Default::default()
        };
        let usage = crate::types::Usage {
            input: 1_000_000,
            output: 100_000,
            cache_read: 2_000_000,
            cache_write: 400_000,
            total_tokens: 0,
        };
        // 3.0 + 1.5 + 0.6 + 1.5 = 6.6
        assert!((cost.cost_usd(&usage) - 6.6).abs() < 1e-9);
        // zero rates (default) => zero cost
        assert_eq!(CostConfig::default().cost_usd(&usage), 0.0);
    }

    #[test]
    fn test_new_generation_presets() {
        let fable = ModelConfig::claude_fable_5();
        assert_eq!(fable.id, "claude-fable-5");
        assert_eq!(fable.api, ApiProtocol::AnthropicMessages);
        assert_eq!(fable.context_window, 1_000_000);
        assert_eq!(fable.cost.as_ref().unwrap().input_per_million, 10.0);
        assert_eq!(fable.cost.as_ref().unwrap().output_per_million, 50.0);

        // Fable 5.1 differs from Fable 5 in exactly one rate: cache hits at
        // 0.025x input rather than 0.1x (#170).
        let fable_5_1 = ModelConfig::claude_fable_5_1();
        assert_eq!(fable_5_1.id, "claude-fable-5-1");
        assert_eq!(fable_5_1.api, ApiProtocol::AnthropicMessages);
        assert_eq!(fable_5_1.context_window, 1_000_000);
        assert_eq!(fable_5_1.max_tokens, 64_000);
        let (c51, c5) = (fable_5_1.cost.unwrap(), fable.cost.unwrap());
        assert_eq!(c51.input_per_million, 10.0);
        assert_eq!(c51.output_per_million, 50.0);
        assert_eq!(c51.cache_write_per_million, 12.5);
        assert_eq!(c51.cache_read_per_million, 0.25);
        assert_eq!(c5.cache_read_per_million, 1.0);
        assert_eq!(c51.input_per_million, c5.input_per_million);
        assert_eq!(c51.output_per_million, c5.output_per_million);
        assert_eq!(c51.cache_write_per_million, c5.cache_write_per_million);

        // Opus 5.5: cheaper than Opus 5 in every column, and cache hits at
        // 0.05x input rather than 0.1x.
        let opus_5_5 = ModelConfig::claude_opus_5_5();
        assert_eq!(opus_5_5.id, "claude-opus-5-5");
        assert_eq!(opus_5_5.name, "Claude Opus 5.5");
        assert_eq!(opus_5_5.api, ApiProtocol::AnthropicMessages);
        assert_eq!(opus_5_5.context_window, 1_000_000);
        assert_eq!(opus_5_5.max_tokens, 64_000);
        let c55 = opus_5_5.cost.as_ref().unwrap();
        assert_eq!(c55.input_per_million, 4.0);
        assert_eq!(c55.output_per_million, 20.0);
        assert_eq!(c55.cache_read_per_million, 0.2);
        assert_eq!(c55.cache_write_per_million, 5.0);
        let compat = opus_5_5.anthropic.as_ref().unwrap();
        assert!(compat.adaptive_thinking);
        assert!(!compat.bearer_auth);
        assert!(compat.native_structured_output);

        let opus_5 = ModelConfig::claude_opus_5();
        assert_eq!(opus_5.id, "claude-opus-5");
        assert_eq!(opus_5.api, ApiProtocol::AnthropicMessages);
        assert_eq!(opus_5.context_window, 1_000_000);
        assert_eq!(opus_5.max_tokens, 64_000);
        assert_eq!(opus_5.cost.as_ref().unwrap().input_per_million, 5.0);
        assert_eq!(opus_5.cost.as_ref().unwrap().output_per_million, 25.0);
        // Derived rates: cache reads bill at 0.1x input, writes at 1.25x.
        assert_eq!(opus_5.cost.as_ref().unwrap().cache_read_per_million, 0.5);
        assert_eq!(opus_5.cost.as_ref().unwrap().cache_write_per_million, 6.25);

        let opus = ModelConfig::claude_opus_4_8();
        assert_eq!(opus.id, "claude-opus-4-8");
        assert_eq!(opus.context_window, 1_000_000);
        assert_eq!(opus.cost.as_ref().unwrap().input_per_million, 5.0);

        let sonnet = ModelConfig::claude_sonnet_5();
        assert_eq!(sonnet.id, "claude-sonnet-5");
        assert_eq!(sonnet.cost.as_ref().unwrap().output_per_million, 10.0);

        let haiku = ModelConfig::claude_haiku_4_5();
        assert_eq!(haiku.id, "claude-haiku-4-5");
        assert_eq!(haiku.context_window, 200_000);

        let gpt = ModelConfig::gpt_5_5();
        assert_eq!(gpt.id, "gpt-5.5");
        assert_eq!(gpt.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(gpt.context_window, 1_000_000);
        assert_eq!(gpt.cost.as_ref().unwrap().output_per_million, 30.0);
        let compat = gpt.compat.as_ref().unwrap();
        assert_eq!(compat.max_reasoning_effort, ReasoningEffortCeiling::XHigh);
        assert!(compat.supports_effort_none);
        // The rest of the compat is native OpenAI's, unchanged.
        assert!(compat.supports_reasoning_effort && compat.supports_prompt_cache_key);
        assert_eq!(compat.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    }

    #[test]
    fn gpt_6_presets() {
        for (mc, id, none) in [
            (ModelConfig::gpt_6_astra(), "gpt-6-astra", false),
            (ModelConfig::gpt_6_sol(), "gpt-6-sol", true),
            (ModelConfig::gpt_6_luna(), "gpt-6-luna", true),
        ] {
            assert_eq!(mc.id, id);
            // Tool calling on GPT-6 needs Responses (Chat Completions refuses
            // it on Astra, and allows it only at effort `none` on Sol/Luna).
            assert_eq!(mc.api, ApiProtocol::OpenAiResponses, "{id}");
            assert_eq!(mc.provider, "openai");
            assert_eq!(mc.base_url, "https://api.openai.com/v1");
            assert!(mc.reasoning);
            assert_eq!(mc.context_window, 1_050_000);
            assert!(mc.max_tokens <= 128_000);
            let compat = mc.compat.as_ref().unwrap();
            assert_eq!(compat.max_reasoning_effort, ReasoningEffortCeiling::Max);
            assert_eq!(compat.supports_effort_none, none, "{id}");
            let tiers = &mc.cost.as_ref().unwrap().context_tiers;
            assert_eq!(tiers.len(), 1, "{id}");
            assert_eq!(tiers[0].above_prompt_tokens, 272_000);
        }
    }

    fn usage(input: u64, cache_read: u64, cache_write: u64, output: u64) -> crate::types::Usage {
        crate::types::Usage {
            input,
            output,
            cache_read,
            cache_write,
            total_tokens: input + output + cache_read + cache_write,
        }
    }

    /// Every rate of every band, checked by pricing one million tokens of
    /// each kind: the dollar figures are OpenAI's published per-1M rates.
    #[test]
    fn gpt_6_rates_per_band() {
        // (preset, short in/cached/write/out, long in/cached/write/out)
        type Band = [f64; 4];
        let cases: [(ModelConfig, Band, Band); 3] = [
            (
                ModelConfig::gpt_6_astra(),
                [10.0, 1.0, 12.5, 50.0],
                [20.0, 2.0, 25.0, 75.0],
            ),
            (
                ModelConfig::gpt_6_sol(),
                [2.0, 0.2, 2.5, 10.0],
                [4.0, 0.4, 5.0, 15.0],
            ),
            (
                ModelConfig::gpt_6_luna(),
                [0.1, 0.01, 0.125, 0.5],
                [0.2, 0.02, 0.25, 0.75],
            ),
        ];
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        for (mc, short, long) in cases {
            let cost = mc.cost.as_ref().unwrap();
            // Short band: 100K of one kind, scaled to per-1M.
            let per_m = |u| cost.cost_usd(&u) * 10.0;
            assert!(close(per_m(usage(100_000, 0, 0, 0)), short[0]), "{}", mc.id);
            assert!(close(per_m(usage(0, 100_000, 0, 0)), short[1]), "{}", mc.id);
            assert!(close(per_m(usage(0, 0, 100_000, 0)), short[2]), "{}", mc.id);
            assert!(close(per_m(usage(0, 0, 0, 100_000)), short[3]), "{}", mc.id);
            // Long band: a 1M-token prompt of one kind.
            let m = 1_000_000;
            assert!(
                close(cost.cost_usd(&usage(m, 0, 0, 0)), long[0]),
                "{}",
                mc.id
            );
            assert!(
                close(cost.cost_usd(&usage(0, m, 0, 0)), long[1]),
                "{}",
                mc.id
            );
            assert!(
                close(cost.cost_usd(&usage(0, 0, m, 0)), long[2]),
                "{}",
                mc.id
            );
            assert!(
                close(
                    cost.cost_usd(&usage(300_000, 0, 0, m)),
                    0.3 * long[0] + long[3]
                ),
                "{}",
                mc.id
            );
            // The threshold is exclusive: exactly 272K stays short.
            assert!(
                close(cost.cost_usd(&usage(272_000, 0, 0, 0)), 0.272 * short[0]),
                "{}",
                mc.id
            );
        }
    }

    #[test]
    fn gpt_5_5_long_band() {
        let cost = ModelConfig::gpt_5_5().cost.unwrap();
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // Short band unchanged: $5 in, $0.50 cached, $30 out, no cache-write charge.
        assert!(close(cost.cost_usd(&usage(200_000, 0, 0, 0)), 1.0));
        assert!(close(cost.cost_usd(&usage(0, 200_000, 0, 0)), 0.1));
        assert!(close(cost.cost_usd(&usage(0, 0, 0, 100_000)), 3.0));
        assert!(close(cost.cost_usd(&usage(272_000, 0, 0, 0)), 1.36));
        // Above 272K: 2x input, 1.5x output — $10 / $45.
        assert!(close(cost.cost_usd(&usage(1_000_000, 0, 0, 0)), 10.0));
        assert!(close(
            cost.cost_usd(&usage(300_000, 0, 0, 1_000_000)),
            3.0 + 45.0
        ));
        // Cached input above 272K: $1.00 (UNVERIFIED — see `gpt_5_5` docs).
        // What matters structurally is that it is not $0.
        assert!(close(cost.cost_usd(&usage(0, 1_000_000, 0, 0)), 1.0));
        assert_eq!(cost.cache_write_per_million, 0.0);
        assert_eq!(cost.context_tiers[0].cache_write_per_million, 0.0);
    }

    #[test]
    fn test_opencode_zen_protocol_selection() {
        // GPT models → Responses API
        let gpt = ModelConfig::opencode_zen("gpt-5.5");
        assert_eq!(gpt.api, ApiProtocol::OpenAiResponses);
        assert_eq!(gpt.provider, "opencode-zen");
        assert_eq!(gpt.base_url, "https://opencode.ai/zen/v1");

        // Grok and Muse Spark models are also Responses-only on Zen
        for id in ["grok-4.7", "grok-build-0.1", "muse-spark-1.3"] {
            let config = ModelConfig::opencode_zen(id);
            assert_eq!(config.api, ApiProtocol::OpenAiResponses, "{id}");
            assert!(config.reasoning, "{id}");
            assert!(config.compat.is_none(), "{id}");
            assert!(config.anthropic.is_none(), "{id}");
        }

        // Claude and Qwen models → Anthropic Messages with Bearer auth
        for id in ["claude-sonnet-5", "qwen3.7-max"] {
            let config = ModelConfig::opencode_zen(id);
            assert_eq!(config.api, ApiProtocol::AnthropicMessages, "{id}");
            let compat = config.anthropic.expect("anthropic compat set");
            assert!(compat.bearer_auth);
        }

        // Everything else → Chat Completions
        for id in ["deepseek-v4-pro", "minimax-m3", "glm-5.2", "kimi-k2.7-code"] {
            let config = ModelConfig::opencode_zen(id);
            assert_eq!(config.api, ApiProtocol::OpenAiCompletions, "{id}");
            assert!(config.compat.is_some());
        }

        // Unsupported families fall through to Chat Completions (with a warning).
        for id in ["gemini-3.5-pro", "jev-1"] {
            let config = ModelConfig::opencode_zen(id);
            assert_eq!(config.api, ApiProtocol::OpenAiCompletions, "{id}");
        }
    }

    /// OpenCode Claude routes carry the same thinking mode and structured-output
    /// support as the matching direct-API presets: without native structured
    /// output, `prompt_structured` forces a tool, which Fable 5.1 and Opus 5.5
    /// reject with a 400.
    #[test]
    fn opencode_claude_compat_matches_presets() {
        for preset in [
            ModelConfig::claude_fable_5(),
            ModelConfig::claude_fable_5_1(),
            ModelConfig::claude_opus_5_5(),
            ModelConfig::claude_opus_5(),
            ModelConfig::claude_opus_4_8(),
            ModelConfig::claude_sonnet_5(),
            ModelConfig::claude_haiku_4_5(),
        ] {
            let want = preset.anthropic.unwrap();
            let got = ModelConfig::opencode_zen(preset.id.clone())
                .anthropic
                .unwrap();
            assert!(got.bearer_auth, "{}", preset.id);
            assert_eq!(
                got.adaptive_thinking, want.adaptive_thinking,
                "{}",
                preset.id
            );
            assert_eq!(
                got.native_structured_output, want.native_structured_output,
                "{}",
                preset.id
            );
        }

        // Non-Claude Anthropic-protocol families keep the defaults: adaptive,
        // tool-forced structured output.
        let qwen = ModelConfig::opencode_zen("qwen3.7-max").anthropic.unwrap();
        assert!(qwen.bearer_auth && qwen.adaptive_thinking && !qwen.native_structured_output);
        let minimax = ModelConfig::opencode_go("minimax-m3").anthropic.unwrap();
        assert!(minimax.bearer_auth && minimax.adaptive_thinking);
        assert!(!minimax.native_structured_output);
    }

    #[test]
    fn claude_id_compat_by_generation() {
        // (id, adaptive thinking, native structured output)
        for (id, adaptive, native) in [
            ("claude-3-5-sonnet-20241022", false, false),
            ("claude-3-7-sonnet", false, false),
            ("claude-sonnet-4", false, false),
            ("claude-sonnet-4-20250514", false, false),
            ("claude-opus-4-1", false, false),
            ("claude-opus-4-5", false, true),
            ("claude-sonnet-4.5", false, true),
            ("claude-haiku-4-5-20251001", false, true),
            ("claude-sonnet-4-6", true, true),
            ("claude-opus-4-8", true, true),
            ("claude-sonnet-5", true, true),
            ("claude-fable-5-1", true, true),
            ("Claude-Opus-5-5", true, true),
            ("claude-nextgen", true, true),
        ] {
            let c = AnthropicCompat::for_claude_id(id);
            assert_eq!(c.adaptive_thinking, adaptive, "{id}");
            assert_eq!(c.native_structured_output, native, "{id}");
            assert!(!c.bearer_auth, "{id}");
        }
    }

    #[test]
    fn test_opencode_go_protocol_selection() {
        // GPT, Grok and Muse Spark models → Responses API
        for id in [
            "gpt-6-luna",
            "gpt-5.6-luna",
            "grok-4.7",
            "grok-4.6",
            "muse-spark-1.3-contributor",
        ] {
            let config = ModelConfig::opencode_go(id);
            assert_eq!(config.api, ApiProtocol::OpenAiResponses, "{id}");
            assert_eq!(config.base_url, "https://opencode.ai/zen/go/v1");
            assert!(config.compat.is_none(), "{id}");
        }

        // Qwen and MiniMax models → Anthropic Messages with Bearer auth
        for id in ["qwen3.7-max", "minimax-m3"] {
            let config = ModelConfig::opencode_go(id);
            assert_eq!(config.api, ApiProtocol::AnthropicMessages, "{id}");
            assert_eq!(config.base_url, "https://opencode.ai/zen/go/v1");
            assert!(config.anthropic.expect("anthropic compat set").bearer_auth);
        }

        // Everything else → Chat Completions
        for id in [
            "glm-5.2",
            "kimi-k2.7-code",
            "deepseek-v4-flash",
            "mimo-v2.5",
        ] {
            let config = ModelConfig::opencode_go(id);
            assert_eq!(config.api, ApiProtocol::OpenAiCompletions, "{id}");
            assert_eq!(config.provider, "opencode-go", "{id}");
        }
    }

    #[test]
    fn test_model_config_openai() {
        let config = ModelConfig::openai("gpt-4o", "GPT-4o");
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        let compat = config.compat.unwrap();
        assert!(compat.supports_store);
        assert!(compat.supports_developer_role);
        assert_eq!(compat.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    }

    #[test]
    fn test_openai_compat_variants() {
        let xai = OpenAiCompat::xai();
        assert_eq!(xai.thinking_format, ThinkingFormat::Xai);
        assert!(!xai.supports_store);

        let groq = OpenAiCompat::groq();
        assert!(groq.supports_usage_in_streaming);
        assert!(!groq.supports_store);

        let deepseek = OpenAiCompat::deepseek();
        assert_eq!(deepseek.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(deepseek.supports_reasoning_effort);
        assert!(deepseek.supports_thinking_control);

        let zai = OpenAiCompat::zai();
        assert!(zai.supports_usage_in_streaming);
        assert!(!zai.supports_store);

        let minimax = OpenAiCompat::minimax();
        assert!(minimax.supports_usage_in_streaming);
        assert!(!minimax.supports_store);

        let ollama = OpenAiCompat::ollama();
        assert!(ollama.requires_assistant_after_tool_result);
        assert!(!ollama.requires_tool_result_name);

        let qwen = OpenAiCompat::qwen();
        assert_eq!(qwen.thinking_format, ThinkingFormat::Qwen);
        assert_eq!(qwen.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(qwen.supports_usage_in_streaming);
        assert!(!qwen.supports_reasoning_effort);
        assert!(!qwen.supports_thinking_control);
    }

    #[test]
    fn test_model_config_deserializes_without_anthropic_field() {
        // Configs persisted before 0.9.0 have no `anthropic` field.
        let mut value = serde_json::to_value(ModelConfig::anthropic("m", "M")).unwrap();
        value.as_object_mut().unwrap().remove("anthropic");
        let config: ModelConfig = serde_json::from_value(value).unwrap();
        assert!(config.anthropic.is_none());
    }

    #[test]
    fn test_anthropic_compat_deserializes_from_partial_json() {
        // Container-level serde(default): missing fields use Default (adaptive on).
        let compat: AnthropicCompat = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(compat.adaptive_thinking);
        assert!(!compat.bearer_auth);

        let compat: AnthropicCompat =
            serde_json::from_value(serde_json::json!({"bearer_auth": true})).unwrap();
        assert!(compat.adaptive_thinking);
        assert!(compat.bearer_auth);
        // Configs persisted before the flag existed load with it off, so
        // they keep tool-forcing exactly as before.
        assert!(!compat.native_structured_output);
        assert!(!AnthropicCompat::default().native_structured_output);
        assert!(!AnthropicCompat::legacy().native_structured_output);
    }

    /// Every Claude preset names a model that the structured-outputs page
    /// lists as supporting `output_config.format`, so each opts into it.
    /// A bare `ModelConfig::anthropic(..)` does not.
    #[test]
    fn claude_presets_use_native_structured_output() {
        for mc in [
            ModelConfig::claude_fable_5(),
            ModelConfig::claude_fable_5_1(),
            ModelConfig::claude_opus_5_5(),
            ModelConfig::claude_opus_5(),
            ModelConfig::claude_opus_4_8(),
            ModelConfig::claude_sonnet_5(),
            ModelConfig::claude_haiku_4_5(),
        ] {
            let compat = mc.anthropic.as_ref().expect("preset sets compat");
            assert!(compat.native_structured_output, "{}", mc.id);
            // Adaptive thinking from 4.6 on; Haiku 4.5 accepts only budget
            // thinking and 400s on `type: "adaptive"`.
            assert_eq!(
                compat.adaptive_thinking,
                mc.id != "claude-haiku-4-5",
                "{}",
                mc.id
            );
            assert!(!compat.bearer_auth, "{}", mc.id);
        }
        assert!(ModelConfig::anthropic("claude-x", "X").anthropic.is_none());
    }

    #[test]
    fn test_openai_compat_deserializes_without_assistant_after_tool_result_flag() {
        let compat: OpenAiCompat = serde_json::from_value(serde_json::json!({
            "supports_store": false,
            "supports_developer_role": false,
            "supports_reasoning_effort": false,
            "supports_thinking_control": false,
            "supports_usage_in_streaming": true,
            "max_tokens_field": "max_tokens",
            "requires_tool_result_name": false,
            "thinking_format": "open_ai"
        }))
        .unwrap();

        assert!(!compat.requires_assistant_after_tool_result);
    }

    #[test]
    fn test_model_config_local_remains_neutral() {
        let config = ModelConfig::local("http://localhost:1234/v1", "local-model");
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(config.provider, "local");
        assert_eq!(config.base_url, "http://localhost:1234/v1");
        let compat = config.compat.unwrap();
        assert!(!compat.requires_assistant_after_tool_result);
    }

    #[test]
    fn test_model_config_ollama() {
        let config = ModelConfig::ollama("http://localhost:11434/v1", "llama3.1:8b");
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(config.provider, "ollama");
        assert_eq!(config.id, "llama3.1:8b");
        assert_eq!(config.name, "llama3.1:8b");
        assert_eq!(config.base_url, "http://localhost:11434/v1");
        let compat = config.compat.unwrap();
        assert!(compat.requires_assistant_after_tool_result);
    }

    #[test]
    fn test_model_config_openai_compat() {
        let config = ModelConfig::openai_compat(
            "http://localhost:1234/v1",
            "qwen3-local",
            "qwen",
            OpenAiCompat::qwen(),
        );
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(config.provider, "qwen");
        assert_eq!(config.id, "qwen3-local");
        assert_eq!(config.name, "qwen3-local");
        assert_eq!(config.base_url, "http://localhost:1234/v1");
        let compat = config.compat.unwrap();
        assert_eq!(compat.thinking_format, ThinkingFormat::Qwen);
    }

    #[test]
    fn test_model_config_qwen() {
        let config = ModelConfig::qwen("qwen3.6-plus", "Qwen 3.6 Plus");
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(config.provider, "qwen");
        assert_eq!(
            config.base_url,
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1"
        );
        assert!(config.reasoning);
        let compat = config.compat.unwrap();
        assert_eq!(compat.thinking_format, ThinkingFormat::Qwen);
        assert_eq!(compat.max_tokens_field, MaxTokensField::MaxTokens);
    }

    #[test]
    fn test_model_config_zai() {
        let config = ModelConfig::zai("glm-4.7", "GLM 4.7");
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(config.provider, "zai");
        assert_eq!(config.base_url, "https://api.z.ai/api/paas/v4");
        assert!(config.compat.is_some());
    }

    #[test]
    fn test_model_config_minimax() {
        let config = ModelConfig::minimax("MiniMax-M3", "MiniMax M3");
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(config.provider, "minimax");
        assert_eq!(config.base_url, "https://api.minimax.io/v1");
        assert_eq!(config.context_window, 1_000_000);
        assert!(config.compat.is_some());
    }

    #[test]
    fn test_model_config_deepseek() {
        let config = ModelConfig::deepseek("deepseek-v4-flash", "DeepSeek V4 Flash");
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        assert_eq!(config.provider, "deepseek");
        assert_eq!(config.base_url, "https://api.deepseek.com");
        assert_eq!(config.context_window, 1_000_000);
        assert_eq!(config.max_tokens, 384_000);
        assert!(config.reasoning);
        assert!(config.compat.is_some());
    }

    #[test]
    fn test_api_protocol_display() {
        assert_eq!(
            ApiProtocol::AnthropicMessages.to_string(),
            "anthropic_messages"
        );
        assert_eq!(
            ApiProtocol::OpenAiCompletions.to_string(),
            "openai_completions"
        );
        assert_eq!(
            ApiProtocol::GoogleGenerativeAi.to_string(),
            "google_generative_ai"
        );
    }

    #[test]
    fn test_cost_config_default() {
        let cost = CostConfig::default();
        assert_eq!(cost.input_per_million, 0.0);
        assert_eq!(cost.output_per_million, 0.0);
    }

    /// Generic constructors take any model id, so they cannot know a price and
    /// must say so with `None` rather than all-zero rates that read as free
    /// (#172). If one of these gains a real price, move it out of this list
    /// **and** add it to `tests/price_audit.rs`, which is what keeps a
    /// compiled-in price honest.
    #[test]
    fn generic_constructors_are_unpriced() {
        let unpriced = [
            ModelConfig::custom(ApiProtocol::BedrockConverseStream, "p", "u", "m", "M"),
            ModelConfig::mock(),
            ModelConfig::anthropic("claude-x", "X"),
            ModelConfig::openai("gpt-x", "X"),
            ModelConfig::openai_responses("gpt-x", "X"),
            ModelConfig::local("http://localhost:1234/v1", "m"),
            ModelConfig::opencode_zen("claude-sonnet-5"),
            ModelConfig::opencode_zen("gpt-5.5"),
            ModelConfig::opencode_go("glm-5.2"),
            ModelConfig::openai_compat("http://h/v1", "m", "p", OpenAiCompat::default()),
            ModelConfig::ollama("http://localhost:11434/v1", "m"),
            ModelConfig::zai("glm-5", "GLM-5"),
            ModelConfig::minimax("MiniMax-M1", "M1"),
            ModelConfig::qwen("qwen-plus", "Qwen"),
            ModelConfig::xai("grok-4-1-fast", "Grok"),
            ModelConfig::groq("llama-3.3-70b-versatile", "Llama"),
            ModelConfig::deepseek("deepseek-v4-flash", "DeepSeek"),
            ModelConfig::mistral("mistral-large-latest", "Mistral"),
            ModelConfig::google("gemini-3-pro", "Gemini"),
        ];
        for mc in unpriced {
            assert!(
                mc.cost.is_none(),
                "{} ({}) should be unpriced",
                mc.id,
                mc.provider
            );
        }
    }

    #[test]
    fn named_presets_are_priced() {
        for mc in [
            ModelConfig::claude_fable_5(),
            ModelConfig::claude_fable_5_1(),
            ModelConfig::claude_opus_5_5(),
            ModelConfig::claude_opus_5(),
            ModelConfig::claude_opus_4_8(),
            ModelConfig::claude_sonnet_5(),
            ModelConfig::claude_haiku_4_5(),
            ModelConfig::gpt_5_5(),
            ModelConfig::gpt_6_astra(),
            ModelConfig::gpt_6_sol(),
            ModelConfig::gpt_6_luna(),
            ModelConfig::meta("muse-spark-1.2", "Muse Spark 1.2"),
        ] {
            let cost = mc.cost.as_ref();
            assert!(
                cost.is_some_and(CostConfig::is_configured),
                "{} lost its price",
                mc.id
            );
        }
    }

    /// Persisted configs must keep loading across the `Option` change, and
    /// `None` must not be written as `null` (older releases would reject it).
    #[test]
    fn cost_serde_back_compat() {
        let mut v = serde_json::to_value(ModelConfig::claude_sonnet_5()).unwrap();

        // A priced object round-trips to Some with its rates.
        let priced: ModelConfig = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(priced.cost.as_ref().unwrap().input_per_million, 2.0);

        // Legacy all-zero object (what pre-0.19 unpriced presets persisted)
        // meant "unknown", so it loads as None — not as a free model.
        v["cost"] = serde_json::json!({"input_per_million": 0.0, "output_per_million": 0.0});
        let legacy: ModelConfig = serde_json::from_value(v.clone()).unwrap();
        assert!(legacy.cost.is_none());

        // The documented trade-off: a free config written by this release
        // is indistinguishable from that, and also reloads as None.
        let mut free = ModelConfig::mock();
        free.cost = Some(CostConfig::new(0.0, 0.0));
        let reloaded: ModelConfig =
            serde_json::from_value(serde_json::to_value(&free).unwrap()).unwrap();
        assert!(reloaded.cost.is_none());

        // Any single non-zero rate — including only a context tier — is a
        // real price and survives.
        v["cost"] = serde_json::json!({
            "input_per_million": 0.0,
            "output_per_million": 0.0,
            "context_tiers": [{"above_prompt_tokens": 1000, "input_per_million": 1.0,
                               "output_per_million": 2.0}]
        });
        let tiered: ModelConfig = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(tiered.cost.as_ref().unwrap().context_tiers.len(), 1);

        // Missing and null both mean unknown.
        v["cost"] = serde_json::Value::Null;
        let null: ModelConfig = serde_json::from_value(v.clone()).unwrap();
        assert!(null.cost.is_none());
        v.as_object_mut().unwrap().remove("cost");
        let missing: ModelConfig = serde_json::from_value(v).unwrap();
        assert!(missing.cost.is_none());

        // None is omitted on serialize, never `null`.
        let out = serde_json::to_value(ModelConfig::deepseek("d", "D")).unwrap();
        assert!(out.get("cost").is_none(), "{out}");
    }
}
