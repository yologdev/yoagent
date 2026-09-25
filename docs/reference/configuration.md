# Configuration

## AgentLoopConfig

The main configuration for the agent loop:

```rust
pub struct AgentLoopConfig {
    pub provider: Arc<dyn StreamProvider>,
    pub model: String,
    pub api_key: String,
    pub thinking_level: ThinkingLevel,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub model_config: Option<ModelConfig>,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    pub get_steering_messages: Option<GetMessagesFn>,
    pub get_follow_up_messages: Option<GetMessagesFn>,
    pub context_config: Option<ContextConfig>,
    pub compaction_strategy: Option<Arc<dyn CompactionStrategy>>,
    pub execution_limits: Option<ExecutionLimits>,
    pub cache_config: CacheConfig,
    pub tool_execution: ToolExecutionStrategy,
    pub retry_config: RetryConfig,
    pub before_turn: Option<BeforeTurnFn>,
    pub after_turn: Option<AfterTurnFn>,
    pub on_error: Option<OnErrorFn>,
    pub input_filters: Vec<Arc<dyn InputFilter>>,
    pub turn_delay: Option<Duration>,
}
```

## StreamConfig

Passed to `StreamProvider::stream()`:

```rust
pub struct StreamConfig {
    pub model: String,
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub thinking_level: ThinkingLevel,
    pub api_key: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub model_config: Option<ModelConfig>,
    pub cache_config: CacheConfig,
}
```

## ContextConfig

Controls context window compaction:

```rust
pub struct ContextConfig {
    pub max_context_tokens: usize,                            // Default: 100,000
    pub system_prompt_tokens: usize,                          // Default: 4,000
    pub keep_recent: usize,                                   // Default: 10
    pub keep_first: usize,                                    // Default: 2
    pub tool_output_max_lines: usize,                         // Default: 200
    pub tool_output_max_lines_overrides: HashMap<String, usize>, // Default: {"read_file": MAX}
    pub compact_target_ratio: f32,                            // Default: 0.7
    pub compact_headroom_turns: Option<usize>,                // Default: Some(30)
    pub truncate_tool_output_on_append: bool,                 // Default: true
}
```

`compact_headroom_turns` sets the compaction target from observed growth (`target = budget − turns × growth_per_turn`), keeping the interval between compactions constant as a session lengthens; `compact_target_ratio` is the fallback and a ceiling on retention. `truncate_tool_output_on_append` caps tool output as it enters the context rather than retroactively, and `tool_output_max_lines_overrides` gives per-tool budgets so a tool that head+tail would damage (a paging reader) can opt out. All three exist to keep the provider's prefix cache intact — see [Context Management](../concepts/context-management.md#prefix-cache-stability).

When `context_config` is not explicitly set, it is automatically derived from `ModelConfig.context_window` (80% for context, 20% reserved for output). If neither is set, `ContextConfig::default()` (100K) is used.

```rust
// Derive from a model's context window:
let config = ContextConfig::from_context_window(200_000);
// config.max_context_tokens == 160_000
```

## ExecutionLimits

Prevents runaway agents:

```rust
#[non_exhaustive]                      // build with Default::default() + with_*
pub struct ExecutionLimits {
    pub max_turns: usize,              // Default: 50
    pub max_total_tokens: usize,       // Default: 1,000,000
    pub max_duration: Duration,        // Default: 600s
    pub max_consecutive_identical_tool_calls: Option<usize>,  // Default: Some(3)
}
```

```rust
ExecutionLimits::default()
    .with_max_turns(20)
    .with_max_consecutive_identical_tool_calls(None)  // disable loop detection
```

## ThinkingLevel

```rust
#[non_exhaustive]  // match with a wildcard arm
pub enum ThinkingLevel {
    Off,        // No thinking (default)
    Minimal,    // Anthropic: effort "low" (adaptive) / 1,024-token budget (legacy)
    Low,        // Anthropic: effort "low" / 1,024
    Medium,     // Anthropic: effort "medium" / 2,048
    High,       // Anthropic: effort "high" / 8,192
    XHigh,      // Anthropic: effort "xhigh" / 16,384 (serde: "xhigh")
    Max,        // Anthropic: effort "max" / 30,720
}
```

Where a provider's ladder is shorter, the upper levels are clamped to the
highest value this crate knows the provider accepts, rather than sent as a
value it would reject — except Anthropic's adaptive
effort, which is passed through unclamped: Opus 4.6 / Sonnet 4.6 have no
`xhigh` rung and reject it.

| Level | Anthropic effort | Anthropic legacy / Bedrock budget | OpenAI-compat¹ / Responses / Azure effort | DeepSeek effort² | Gemini 2.x / Vertex budget³ |
|-------|------|------|------|------|------|
| `Minimal`, `Low` | `low` | 1,024 | `low` | `low` | 1,024 |
| `Medium` | `medium` | 2,048 | `medium` | `medium` | 8,192 |
| `High` | `high` | 8,192 | `high` | `high` | 24,576 |
| `XHigh` | `xhigh` | 16,384 | `high` (clamped) | `high` (clamped) | 24,576 (clamped) |
| `Max` | `max` | 30,720 | `high` (clamped) | `max` | 24,576 (clamped) |

¹ OpenAI-compat sends `reasoning_effort` only when `supports_reasoning_effort`
is set. ² The DeepSeek column applies when both `supports_thinking_control` and
`supports_reasoning_effort` are set. DeepSeek maps a requested `xhigh` to
`high` itself, so `XHigh` is sent as `high` and only `Max` selects `max`.
³ Gemini 3 and later get `thinkingConfig.thinkingLevel` instead, never both:
`Minimal` → `MINIMAL`, `Low` → `LOW`, `Medium` → `MEDIUM`, `High` / `XHigh` /
`Max` → `HIGH`. `Minimal` is clamped to `LOW` on models without a `MINIMAL`
rung (3.7 / 3.8 Flash, 3.x Pro, unlisted ids). `Off` sends the lowest accepted
level (`MINIMAL` or `LOW`) because Gemini 3 cannot turn thinking fully off. The
generation is read from the model id; override it with `GoogleCompat`. See
[Google Gemini](../providers/google.md#thinking).

## CostConfig

Token pricing per million:

```rust
#[non_exhaustive]                          // build with new() + with_*, not a literal
pub struct CostConfig {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: f64,
    pub cache_write_per_million: f64,
    pub context_tiers: Vec<ContextTier>,   // empty = one flat rate at every size
}
```

Cache rates are set with builders rather than positionally. Four same-typed
`f64` arguments in a row is a transposition hazard, and no vendor publishes them
in one order — Anthropic lists input / cache-write / cache-read / output, OpenAI
lists input / cached-input / output:

```rust
CostConfig::new(5.0, 30.0)          // input, output — output is always dearer
    .with_cache_read(0.5)
    .with_cache_write(6.25)
```

`ModelConfig::cost` is an `Option<CostConfig>`. `None` means **pricing
unknown**, not free: only the named presets whose rates were checked against the
vendor (`claude_*`, `gpt_5_5`, `meta`) return `Some`; generic constructors such
as `deepseek(id, name)` or `openai(id, name)` cannot know the model's price and
return `None`. Supply one when you know it:

```rust
let mut config = ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash");
config.cost = Some(CostConfig::new(0.15, 0.60));
```

`Some` with every rate zero means **free** — a model you run locally, say — and
the crate's accounting reports `0.0` for it. `session_cost_usd()`,
`SessionStats::cost_usd` and the `llm_stream` span's `cost_usd` are absent only
for `cost: None`, never `$0` for an unknown price:

```rust
let mut config = ModelConfig::local("http://localhost:1234/v1", "qwen3");
config.cost = Some(CostConfig::new(0.0, 0.0)); // free, not unknown
```

To adjust one rate on a named preset, use `get_or_insert_with` rather than
`if let Some(c) = config.cost.as_mut()`, which silently does nothing on a
generic constructor:

```rust
config.cost.get_or_insert_with(CostConfig::default).input_per_million = 1.80;
```

On a generic constructor (`cost: None`) that line creates a `CostConfig` whose
other rates are zero, and zero now means free: output and cache tokens would be
billed at a real `$0`. Price a generic constructor with every rate you pay
instead:

```rust
let mut config = ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash");
// Rates per million tokens; take them from the vendor's pricing page.
config.cost = Some(CostConfig::new(INPUT, OUTPUT).with_cache_read(CACHE_READ));
```

**Persistence caveat.** Before 0.19, unpriced configs were written with an
all-zero `cost` object. To keep those loading as unknown rather than free, a
`cost` object whose rates are all zero (`is_configured()` false, context tiers
included) deserializes to `None`. A free config saved by 0.19 has the same
encoding, so it too reloads as `None`; set the zero `CostConfig` again after
loading if you persist one.

### Context tiers

Some vendors charge more above a prompt-size threshold. `cost_usd` selects by
the request's **prompt** tokens (`input + cache_read + cache_write`), so a long
reply to a short prompt stays on the base rate:

```rust
CostConfig::new(5.0, 30.0)
    .with_context_tier(ContextTier::new(272_000, 10.0, 45.0).with_cache_read(1.0))
```

Tiers are kept sorted, and `cost_usd` takes the last one the prompt clears, so a
multi-step schedule works. **No shipped preset sets one** — see
`ModelConfig::gpt_5_5`'s docs for why the one candidate stayed flat.

One caveat if you add a tier: prompt size is derived as
`input + cache_read + cache_write`, which holds only where the provider
subtracts cached tokens out of `input`. `bedrock.rs` populates neither cache
field, so a heavily-cached prompt reads small there.

## ModelConfig Presets

yoagent provides first-class `ModelConfig::*` constructors for Anthropic, OpenAI, Google Gemini, xAI, Groq, DeepSeek, Mistral, MiniMax, Z.ai, Qwen, Ollama, and local OpenAI-compatible servers.

See [Model Presets](../providers/model-presets.md) for the full table of constructors, default base URLs, context windows, and DeepSeek legacy alias notes.
