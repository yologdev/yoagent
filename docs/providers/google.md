# Google Gemini Provider

Two providers for Google's Gemini models:

- `GoogleProvider` — Google AI Studio (Generative AI API)
- `GoogleVertexProvider` — Google Cloud Vertex AI

## Google AI Studio

```rust
use yoagent::provider::ModelConfig;

let agent = Agent::from_config(ModelConfig::google("gemini-3.8-flash", "Gemini 3.8 Flash"));
```

### API Details

- **Endpoint**: `{base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse&key={api_key}`
- **Auth**: API key as query parameter
- **Default base URL**: `https://generativelanguage.googleapis.com`
- **Default context window**: 1,000,000 tokens

### Message Format

Google uses a different message format than OpenAI/Anthropic:

| yoagent | Google API |
|----------|-----------|
| `user` role | `user` role |
| `assistant` role | `model` role |
| `Content::Text` | `{"text": "..."}` |
| `Content::Image` | `{"inlineData": {...}}` |
| `Content::ToolCall` | `{"functionCall": {...}}` |
| `Message::ToolResult` | `{"functionResponse": {...}}` |
| System prompt | `systemInstruction` field |
| Tools | `tools[].functionDeclarations[]` |

### Thinking

Both Google AI Studio and Vertex AI map `ThinkingLevel` onto
`generationConfig.thinkingConfig`, using exactly one of two fields:

- **Gemini 3 and later: `thinkingLevel`.** Google documents `thinkingLevel` as
  "Recommended for Gemini 3 or later models. Use with earlier models results
  in an error." `Off` → (omitted), `Minimal` → `MINIMAL`, `Low` → `LOW`,
  `Medium` → `MEDIUM`, `High` / `XHigh` / `Max` → `HIGH`.
- **Gemini 2.x: `thinkingBudget`**, unchanged: 1,024 / 8,192 / 24,576 tokens
  (see the [`ThinkingLevel` table](../reference/configuration.md#thinkinglevel)).

The two are never sent together — Vertex: "If you specify both
thinking_level and thinking_budget in the same request for a Gemini 3 model,
the model returns an error."

**How the generation is detected.** Google's rule is version-based, so the
crate reads the version from the model id: the last path segment is taken (so
`models/gemini-3.8-flash` and
`projects/…/publishers/google/models/gemini-3.1-pro-preview` both work), an
`@version` suffix is dropped, and a `gemini-<major>[.<minor>]` id with major ≥ 3
gets `thinkingLevel`. `gemini-flash-latest` and `gemini-pro-latest` also get
it (the changelog points them at 3.5 Flash and 3 Pro preview).
`gemini-flash-lite-latest`, and any id the rule cannot read (`gemma-*`,
tuned-model names, proxy aliases), keeps `thinkingBudget`, which Gemini 3
still accepts for backward compatibility.

**`MINIMAL` is not accepted everywhere.** Per Google's level table, 3.8 and
3.7 Flash reject it and 3.1 Pro does not support it. The crate sends `MINIMAL`
only where it is documented as accepted — 3.6 / 3.5 Flash, every 3.x
Flash-Lite, 3 Flash — and clamps `Minimal` to `LOW` everywhere else, including
the `-latest` aliases, which Google swaps to new models without notice.

**`Off` sends nothing — and does not disable thinking on Gemini 3.** On
every Gemini model `Off` omits `thinkingConfig`. On 2.x that is the crate's
long-standing behaviour. On Gemini 3 thinking cannot be switched off this way:
with no `thinkingConfig` the model thinks at its own **default** level —
`HIGH` on 3.1 Pro, `MEDIUM` on 3.5–3.8 Flash, `MINIMAL` on 3.x Flash-Lite
(`HIGH` on 3 Flash) — per Google's level tables. Google: "If you don't
specify a thinking level, Gemini will use the Gemini 3 models' default
thinking level (e.g., "high" for Gemini 3.1 Pro, and "medium" for Gemini 3.5
Flash)", and "You cannot disable thinking for Gemini 3.1 Pro. Gemini 3 Flash
and Flash-Lite also do not support full thinking-off."

**To ask for the least thinking, use `ThinkingLevel::Minimal`.** It sends
`MINIMAL` where the model accepts it — which Google says "Matches the "no
thinking" setting for most queries" but "does not guarantee that thinking is
off" — and `LOW` elsewhere.

**Image and TTS models.** Google's Vertex AI thinking table lists narrower
level sets for the image models: Gemini 3.1 Flash Image and 3.1 Flash-Lite
Image accept "MINIMAL , HIGH", and Gemini 3 Pro Image accepts "HIGH" only.
The crate reads any Gemini 3+ `-image` id that way: on the `MINIMAL`/`HIGH`
models `Minimal` and `Low` send `MINIMAL` and `Medium` and above send `HIGH`;
on `-pro…-image` ids every level sends `HIGH`. Gemini 3 TTS models
(`gemini-3.8-flash-tts`, `gemini-3.8-flash-lite-tts`,
`gemini-3.1-flash-tts-preview`) are not in either guide's list of thinking
models, so they get no `thinkingConfig` at any level. 2.x image and TTS ids
keep the `thinkingBudget` payload unchanged.

**Override.** When the id does not say what the model is, set `GoogleCompat`:

```rust
use yoagent::provider::{GoogleCompat, ModelConfig};

let mut config = ModelConfig::google("my-gemini-proxy-alias", "Gemini via proxy");
config.google = Some(GoogleCompat::force_thinking_level()); // or force_thinking_budget()
```

A forced `thinkingLevel` on an unrecognised id clamps `Minimal` to `LOW`,
since the crate cannot know whether that model accepts `MINIMAL`; `Off` still
sends nothing.

### Streaming

Uses SSE format (`alt=sse`). Each chunk contains `candidates` with `content.parts` and optional `usageMetadata`.

## Google Vertex AI

`GoogleVertexProvider` uses the same message format but with Vertex AI authentication and endpoints.

- **Protocol**: `ApiProtocol::GoogleVertex`
- **Auth**: OAuth2 / service account credentials
- **Endpoint pattern**: `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:streamGenerateContent`
