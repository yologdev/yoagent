# Model Presets

yoagent's first-class model presets are the `ModelConfig::*` constructors. A preset sets provider routing, base URL, context metadata, default output limits, and provider compatibility flags.

Use a preset when the provider is listed here. Use a custom `ModelConfig` when you need a compatible provider or endpoint that does not have a constructor yet.

## First-Class Constructors

| Constructor | Provider | Protocol | Default Base URL | Context | Default Max Output |
|-------------|----------|----------|------------------|---------|--------------------|
| `ModelConfig::anthropic(id, name)` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 200K | 16,000 |
| `ModelConfig::claude_fable_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_fable_5_1()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_opus_5_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_opus_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_opus_4_8()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_sonnet_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_sonnet_5_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_sonnet_4_6()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_haiku_5_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_haiku_4_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 200K | 32,000 |
| `ModelConfig::openai(id, name)` | OpenAI | `OpenAiCompletions` | `https://api.openai.com/v1` | 128K | 4,096 |
| `ModelConfig::openai_responses(id, name)` | OpenAI | `OpenAiResponses` | `https://api.openai.com/v1` | 128K | 16,000 |
| `ModelConfig::gpt_5_5()` | OpenAI | `OpenAiCompletions` | `https://api.openai.com/v1` | 1M | 64,000 |
| `ModelConfig::gpt_6_astra()` | OpenAI | `OpenAiResponses` | `https://api.openai.com/v1` | 1,050,000 | 64,000 |
| `ModelConfig::gpt_6_sol()` | OpenAI | `OpenAiResponses` | `https://api.openai.com/v1` | 1,050,000 | 64,000 |
| `ModelConfig::gpt_6_1_sol()` | OpenAI | `OpenAiResponses` | `https://api.openai.com/v1` | 1,050,000 | 64,000 |
| `ModelConfig::gpt_6_1_terra()` | OpenAI | `OpenAiResponses` | `https://api.openai.com/v1` | 1,050,000 | 64,000 |
| `ModelConfig::gpt_6_luna()` | OpenAI | `OpenAiResponses` | `https://api.openai.com/v1` | 1,050,000 | 64,000 |
| `ModelConfig::opencode_zen(model_id)` | OpenCode Zen | by model family | `https://opencode.ai/zen/v1` | 128K | 16,000 |
| `ModelConfig::opencode_go(model_id)` | OpenCode Go | by model family | `https://opencode.ai/zen/go/v1` | 128K | 16,000 |
| `ModelConfig::google(id, name)` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1M | 8,192 |
| `ModelConfig::gemini_4_argon()` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1,048,576 | 65,536 |
| `ModelConfig::gemini_3_8_flash()` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1,048,576 | 65,536 |
| `ModelConfig::gemini_3_6_flash()` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1,048,576 | 65,536 |
| `ModelConfig::gemini_3_5_flash_lite()` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1,048,576 | 65,536 |
| `ModelConfig::gemini_3_1_flash_lite()` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1,048,576 | 65,536 |
| `ModelConfig::gemini_3_flash_preview()` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1,048,576 | 65,536 |
| `ModelConfig::gemini_3_1_pro_preview()` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1,048,576 | 65,536 |
| `ModelConfig::xai(id, name)` | xAI | `OpenAiCompletions` | `https://api.x.ai/v1` | 131,072 | 4,096 |
| `ModelConfig::grok_4_7()` | xAI | `OpenAiCompletions` | `https://api.x.ai/v1` | 500,000 | 64,000 |
| `ModelConfig::groq(id, name)` | Groq | `OpenAiCompletions` | `https://api.groq.com/openai/v1` | 128K | 4,096 |
| `ModelConfig::deepseek(id, name)` | DeepSeek | `OpenAiCompletions` | `https://api.deepseek.com` | 1M | 384K |
| `ModelConfig::mistral(id, name)` | Mistral | `OpenAiCompletions` | `https://api.mistral.ai/v1` | 128K | 4,096 |
| `ModelConfig::mistral_large_4()` | Mistral | `OpenAiCompletions` | `https://api.mistral.ai/v1` | 1M | 32,768 |
| `ModelConfig::minimax(id, name)` | MiniMax | `OpenAiCompletions` | `https://api.minimax.io/v1` | 1M | 4,096 |
| `ModelConfig::meta(id, name)` | Meta (Muse Spark, Muse Wonder) — US-only preview as of 2026-07 | `OpenAiCompletions` | `https://api.meta.ai/v1` | 1M | 131,072 |
| `ModelConfig::zai(id, name)` | Z.ai | `OpenAiCompletions` | `https://api.z.ai/api/paas/v4` | 128K | 4,096 |
| `ModelConfig::glm_5_3()`, `glm_5_3_flash()`, `glm_5_3_flashx()` | Z.ai | `OpenAiCompletions` | `https://api.z.ai/api/paas/v4` | 1M | 128,000 |
| `ModelConfig::qwen(id, name)` | Qwen / DashScope | `OpenAiCompletions` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | 128K | 4,096 |
| `ModelConfig::qwen_3_8_max()`, `qwen_3_8_flash()`, `qwen_3_8_omni_flash()` | Qwen / DashScope | `OpenAiCompletions` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | 1M | 131,072 |
| `ModelConfig::ollama(base_url, model_id)` | Ollama | `OpenAiCompletions` | caller provided | 128K | 4,096 |
| `ModelConfig::openai_compat(base_url, model_id, provider, compat)` | Custom compatible server | `OpenAiCompletions` | caller provided | 128K | 4,096 |
| `ModelConfig::local(base_url, model_id)` | Local compatible server | `OpenAiCompletions` | caller provided | 128K | 4,096 |
| `ModelConfig::custom(api, provider, base_url, model_id, name)` | Any (Azure, Bedrock, Vertex, gateways) | caller provided | caller provided | 128K | 16,000 |

The generic constructors (`google(id, name)`, `xai`, `mistral`, …) assume conservative limits unless the price table states the model's (below); for a model with a named preset, use the preset — it carries the model's real context window and output limit (a generic `ModelConfig::xai("grok-4.7", ..)` compacts at 131K and stops replies at 4,096 tokens). The constructors do not validate model IDs. They send the `id` you pass through to the provider, which lets you use newly released model IDs before yoagent updates its examples.

**Limits from the price table.** A model with no named preset can still
have its real limits: when its entry in the price data
(`src/provider/prices.json`, or a user or fetched table — see
[Model Pricing](../concepts/pricing.md#limits)) states `context_window` and
`max_output`, the generic constructor uses them — the context window, and
`max_tokens` as half of `max_output`, at most 64,000 (the presets' rule), but
never below the provider's own default unless that exceeds `max_output`. The
bundled snapshot's limits apply whether or not pricing is enabled.
`ModelConfig::anthropic("claude-opus-4-6", ..)` gets 1M / 64K this way; so
does `ModelConfig::preset("openrouter", "anthropic/claude-opus-4.6")`. A
preset's own limits win.

When such a config becomes an agent's (`Agent::from_config` / `from_provider` / `set_model`, `SubAgentTool`'s constructors) with both limits still at the generic constructor's defaults and no preset of those limits for that provider and id, yoagent logs one `warn!` per model per process saying the limits are guesses, and pointing at `ModelConfig::preset(..)` when one exists. A config built before the price table listed its model (say, before `enable_live`) still warns, hinting to build it again — or to ignore it if the limits were set on purpose. Setting `context_window` or `max_tokens` (or using the preset) silences it. Only configs a first-party generic constructor built, still at its base URL, are checked: gateways and custom endpoints (`openai_compat`, `custom`, a changed `base_url`, `anthropic` with an id not starting `claude-`) and local servers are not, whatever their `provider` string. The deprecated `Agent::new` + `with_model_config` path does not check.

**Nothing is priced until the process opts in** — `prices::enable_bundled()` (the bundled data file `src/provider/prices.json`, offline) or `prices::enable_live(..)` (native) — see [Model Pricing](../concepts/pricing.md#enabling-pricing). After that, prices are looked up by `(provider, id)` when the config is built. The named presets (`claude_fable_5`, `claude_fable_5_1`, `claude_opus_5_5`, `claude_opus_5`, `claude_opus_4_8`, `claude_sonnet_5`, `claude_sonnet_5_5`, `claude_sonnet_4_6`, `claude_haiku_5_5`, `claude_haiku_4_5`, `gpt_5_5`, `gpt_6_astra`, `gpt_6_sol`, `gpt_6_1_sol`, `gpt_6_1_terra`, `gpt_6_luna`, `gemini_4_argon`, `gemini_3_8_flash`, `gemini_3_6_flash`, `gemini_3_5_flash_lite`, `gemini_3_1_flash_lite`, `gemini_3_flash_preview`, `gemini_3_1_pro_preview`, `grok_4_7`, `mistral_large_4`, `glm_5_3`, `glm_5_3_flash`, `glm_5_3_flashx`, `qwen_3_8_max`, `qwen_3_8_flash`, `qwen_3_8_omni_flash`) are all in the snapshot (`cost: Some(CostConfig)`). The OpenAI ones carry a context tier: above 272K prompt tokens the whole request bills at the long-context rates (`gpt_5_5`'s long-band cached-input rate is unverified; see its `note` in the data file); so do `claude_haiku_5_5` (above 100K), `gemini_3_1_pro_preview` (above 200K) and `grok_4_7` (above 200K; xAI bills a prompt of exactly 200,000 tokens at the higher rate, see its `note`). `gemini_3_1_flash_lite` and `gemini_3_flash_preview` carry the text input rate; Google bills audio input at twice it, so audio input is under-counted. `gemini_4_argon`, `gemini_3_8_flash` and `gemini_3_6_flash` carry the **regular** rate while Google runs a promotion (Gemini 4 Argon through 2026-11-30, 3.8 and 3.6 Flash through 2026-12-31): until then their costs read up to twice the bill, an upper bound, as with DeepSeek's peak rate. The generic first-party constructors (`anthropic`, `openai`, `openai_responses`, `google`, `xai`, `groq`, `deepseek`, `mistral`, `zai`, `minimax`, `qwen`, `meta`) are priced when the file lists the id — `ModelConfig::anthropic("claude-sonnet-5", ..)` gets `claude_sonnet_5()`'s rates, `meta` prices `muse-spark-1.1`/`-1.2`/`-1.3` and `muse-wonder-1.0`, `deepseek` prices `deepseek-flash`/`deepseek-v4-pro` and the legacy `deepseek-v4-flash`/`deepseek-v4-flash-vision-exp` at the peak rate (see [DeepSeek Models](#deepseek-models)) — and carry `cost: None` otherwise. Gateways and custom endpoints (`custom`, `openai_compat`, `local`, `ollama`, `opencode_zen`, `opencode_go`) are always `cost: None`, since what they bill is not the vendor's list price. `None` means pricing **unknown**, not free; set `config.cost = Some(CostConfig::new(input, output))` yourself if you want cost reporting. The OpenCode presets select the API protocol from the model id — see [OpenCode Zen & Go](opencode.md).

The `claude_*` presets also set `AnthropicCompat::native_structured_output`, so `Agent::prompt_structured` uses the API's native JSON outputs instead of a forced tool call. `claude_fable_5_1` and `claude_opus_5_5` depend on it, because both models reject forced `tool_choice`. `ModelConfig::anthropic(id, name)` infers it (and the thinking mode) from a Claude id, and leaves it off for any other id. See [Structured Outputs](../concepts/structured-outputs.md).

## Looking Up a Preset by Name

For a host that takes the model as text (`openai/gpt-6-sol`, a config file, a
flag):

- `ModelConfig::presets()` — every named preset, as built configs.
- `ModelConfig::preset(provider, id)` — the named preset for exactly that
  provider and id (as the preset's `provider` and `id` spell them), else
  `None`. Case-sensitive, no aliases: `("openai", "gpt-6-sol")` finds
  `gpt_6_sol()`; `("openai_responses", "gpt-6-sol")` and `("opencode-zen",
  "gpt-6-sol")` find nothing.
- `ModelConfig::preset("bedrock", id)` — a Claude preset on Amazon Bedrock,
  by Bedrock model id (`anthropic.claude-sonnet-4-6`) or cross-Region
  inference profile id (`us.anthropic.claude-sonnet-4-6`, `global.…`, and the
  `us-gov`, `eu`, `apac`, `jp`, `au`, `ca`, `in` geographies). The config is
  `BedrockConverseStream` on `https://bedrock-runtime.us-east-1.amazonaws.com`
  — set `base_url` to a Region the profile serves — with the Anthropic
  preset's limits and `cost: None`: Bedrock bills its own rates (regional
  and geographic profiles 10% above global ones, per AWS), which yoagent does
  not carry. Bedrock spells Haiku 4.5
  `anthropic.claude-haiku-4-5-20251001-v1:0`; the other Claude presets are
  the Anthropic id after `anthropic.`. ARNs, unknown prefixes and other
  vendors' models find nothing. See [Amazon Bedrock](bedrock.md) for
  thinking on Bedrock.
- `ModelConfig::preset("openrouter", "vendor/model")` — a vendor preset
  behind OpenRouter (`https://openrouter.ai/api/v1`, Chat Completions,
  `OpenAiCompat::openrouter()`, key from `OPENROUTER_API_KEY`), for the
  vendors `anthropic`, `openai`, `google`, `x-ai`, `z-ai` and `qwen`, with
  the vendor preset's limits and `cost: None` (a gateway). The model is the
  vendor's id as OpenRouter lists it, which for Claude writes the version
  with a dot: `anthropic/claude-sonnet-4.6`, `anthropic/claude-opus-5`,
  `openai/gpt-6-sol`, `x-ai/grok-4.7`. A vendor model with no preset whose
  limits the price table states is found too (`anthropic/claude-opus-4.6`).
  The dashed Anthropic spelling and variants such as `:batch` find nothing.
- `ModelConfig::generic(provider, id)` — the provider's generic constructor
  (`anthropic`, `openai`, `google`, `xai`, `groq`, `deepseek`, `mistral`,
  `zai`, `minimax`, `qwen`, `meta`), **never a preset**: the context window
  and output limit are the provider's defaults from the table above, not the
  model's, unless the price table states the model's limits (above). For `openai`, a reasoning id (anything but `gpt-3*`, `gpt-4*`,
  `chatgpt-*`, `*-chat*`) gets `openai_responses`. `None` for any other
  provider (gateways and local servers need a base URL).

```rust
// Split at the first `/` (Groq's and OpenRouter's ids contain one).
let (provider, id) = "groq/openai/gpt-oss-120b".split_once('/').unwrap();
let config = ModelConfig::preset(provider, id).or_else(|| {
    // No preset: the limits are guesses. Say so to the user.
    ModelConfig::generic(provider, id)
});
```

## OpenAI-Compatible Presets

These constructors all use `OpenAiCompatProvider`:

```rust
use yoagent::provider::ModelConfig;

let agent = Agent::from_config(ModelConfig::deepseek(
    "deepseek-flash",
    "DeepSeek Flash",
));
```

OpenAI-compatible presets also set `OpenAiCompat` flags for provider-specific API differences, such as `max_tokens` vs. `max_completion_tokens`, reasoning fields, tool result formatting, and streaming usage support. See [OpenAI Compatible](openai-compat.md) for the full quirk-flag list.

## OpenAI Presets

The OpenAI presets also declare each model's reasoning-effort ceiling
(`OpenAiCompat::max_reasoning_effort`), so `ThinkingLevel::XHigh`/`Max` reach
the model's real top rung instead of being clamped to `high`:

| Preset | Protocol | Effort ceiling | `XHigh` → | `Max` → |
|--------|----------|----------------|-----------|---------|
| `gpt_5_5()` | Chat Completions | `xhigh` | `xhigh` | `xhigh` |
| `gpt_6_astra()`, `gpt_6_sol()`, `gpt_6_1_sol()`, `gpt_6_1_terra()`, `gpt_6_luna()` | Responses | `max` | `xhigh` | `max` |

`ThinkingLevel::Off` omits the effort on every preset, so the model runs at
its own default (`medium` on GPT-5.5, Sol, Terra and Luna) — it does not turn
reasoning off.
The crate never sends OpenAI's `none` rung (GPT-6 Astra, among others, rejects
it with a 400).

The GPT-6 presets use the Responses API because Chat Completions does not
support function calling with GPT-6 Astra, and allows it on Sol/Luna only at
effort `none`. GPT-6 rejects `temperature` while the effort is not `none`, and
this crate never sends `none`, so the presets turn `accepts_sampling_params`
off and the agent loop sends no temperature. The Responses
provider does not enforce `prompt_structured` schemas (it warns and ignores the
schema), so structured outputs on the GPT-6 presets are not guaranteed to
match. There is no bare `gpt-6` model id.

## Ollama Models

Use `ModelConfig::ollama` for Ollama's OpenAI-compatible endpoint:

```rust
let llama = ModelConfig::ollama("http://localhost:11434/v1", "llama3.1:8b");
```

Ollama remains separate from `ModelConfig::local(...)` because some Ollama-served models need an assistant message after tool results, while other local OpenAI-compatible servers may not. The Ollama preset enables that transcript workaround; the generic local preset stays neutral.

## Qwen Models

Use `ModelConfig::qwen` for hosted Qwen / DashScope:

```rust
let qwen = ModelConfig::qwen("qwen3.6-plus", "Qwen 3.6 Plus");
```

The default base URL is the international DashScope endpoint. For other regions, override `base_url` after construction:

```rust
let mut qwen = ModelConfig::qwen("qwen3.6-plus", "Qwen 3.6 Plus");
qwen.base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1".into();
```

Region endpoints:

- International/Singapore: `https://dashscope-intl.aliyuncs.com/compatible-mode/v1`
- China/Beijing: `https://dashscope.aliyuncs.com/compatible-mode/v1`
- US/Virginia: `https://dashscope-us.aliyuncs.com/compatible-mode/v1`

`qwen_3_8_omni_flash()` (an omni model: this crate sends it text and images;
text out; JSON mode but no JSON Schema, so not `Agent::prompt_structured`)
keeps the international base URL, but Alibaba's examples
for it use the workspace endpoint
(`{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com`) and nothing confirms it is
served on `dashscope-intl`; set `base_url` if requests fail.

For locally deployed Qwen, keep the local endpoint and opt into Qwen's model-family compat flags:

```rust
let qwen_local = ModelConfig::openai_compat(
    "http://localhost:1234/v1",
    "qwen3-local",
    "qwen",
    OpenAiCompat::qwen(),
);
```

If a local serving layer also has its own quirks, combine the compat flags explicitly. For example, Qwen served by Ollama may need both Qwen reasoning parsing and Ollama's tool-result transcript workaround:

```rust
let mut compat = OpenAiCompat::qwen();
compat.requires_assistant_after_tool_result = true;

let qwen_ollama = ModelConfig::openai_compat(
    "http://localhost:11434/v1",
    "qwen2.5-coder:7b",
    "ollama",
    compat,
);
```

## DeepSeek Models

Use the current DeepSeek API model IDs:

```rust
let flash = ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"); // V4.1 Flash (1017), takes images
let pro = ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro");
```

**Pricing** (after `prices::enable_bundled()`): `deepseek-flash` and the legacy `deepseek-v4-flash` / `deepseek-v4-flash-vision-exp` at $0.30 input / $0.006 cache hit / $1.20 output per million tokens, `deepseek-v4-pro` at $1.32 / $0.044 / $3.96 — DeepSeek's **peak** rates. Off-peak (every hour but 01:00–04:00 and 06:00–10:00 UTC on weekdays) DeepSeek bills half; a `CostConfig` has no time of day, so reported costs are an upper bound and a `Budget` stops early rather than late. To count off-peak rates, install them as an override ([Model Pricing](../concepts/pricing.md#which-constructors-are-priced)).

The legacy names `deepseek-chat` and `deepseek-reasoner` were discontinued on 2026-07-24; requests using them fail. `deepseek-v4-flash` (and `deepseek-v4-flash-vision-exp`) are still accepted, but the V4 Flash model behind them is retired — DeepSeek serves those requests with V4.1 Flash at the Flash price. Prefer `deepseek-flash`. Since the V4.1-Flash-1017 snapshot, `deepseek-flash` accepts images (`deepseek-v4-pro` does not); yoagent sends them as OpenAI-style `image_url` parts, as for every OpenAI-compatible provider.

yoagent also sends DeepSeek's current request shape:

- `max_tokens`, not `max_completion_tokens`
- `thinking: { "type": "enabled" | "disabled" }`
- `reasoning_effort` when `ThinkingLevel` is not `Off`
- DeepSeek cache hit/miss usage fields when present

Both models default to thinking mode. What used to be `deepseek-chat` is the same model with thinking off:

```rust
let non_thinking = Agent::from_config(ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"))
    .with_thinking(ThinkingLevel::Off);
```

## Compat Flags Without Constructors

`OpenAiCompat` also has quirk presets such as `OpenAiCompat::cerebras()` and `OpenAiCompat::openrouter()`. Those are compatibility profiles, not full `ModelConfig` constructors (for an OpenRouter model with a vendor preset, `ModelConfig::preset("openrouter", "vendor/model")` builds the whole config; see [Looking Up a Preset by Name](#looking-up-a-preset-by-name)). To use them, call `ModelConfig::openai_compat(...)` with the provider name, base URL, and `compat` value you need. The API key is resolved from the provider string, so pass `"cerebras"` / `"openrouter"` as `provider` to pick up `CEREBRAS_API_KEY` / `OPENROUTER_API_KEY`; any other name falls back to `YOAGENT_API_KEY` / `API_KEY` (see [API keys](overview.md#api-keys)).
