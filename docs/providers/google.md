# Google Gemini Provider

Two providers for Google's Gemini models:

- `GoogleProvider` — Google AI Studio (Generative AI API)
- `GoogleVertexProvider` — Google Cloud Vertex AI

## Google AI Studio

```rust
use yoagent::provider::ModelConfig;

let agent = Agent::from_config(ModelConfig::google("gemini-2.5-flash", "Gemini 2.5 Flash"));
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
  in an error." `Minimal` → `MINIMAL`, `Low` → `LOW`, `Medium` → `MEDIUM`,
  `High` / `XHigh` / `Max` → `HIGH`.
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

**`Off` does not disable thinking on Gemini 3.** Omitting `thinkingConfig`
would leave the model at its default level (e.g. `MEDIUM` on 3.5–3.8 Flash, `HIGH`
on 3.1 Pro), so on Gemini 3 `Off` sends the lowest level the model accepts
instead — `MINIMAL`, which Google says "Matches the "no thinking" setting for
most queries" but "does not guarantee that thinking is off", or `LOW` where
`MINIMAL` is not accepted — without `includeThoughts`. Thinking cannot be
turned off at all on Gemini 3 Pro / 3.1 Pro. On 2.x, `Off` still omits
`thinkingConfig`.

Image models (e.g. 3.1 Flash Image, `MINIMAL`/`HIGH` only) are not modelled;
pick a level they accept.

**Override.** When the id does not say what the model is, set `GoogleCompat`:

```rust
use yoagent::provider::{GoogleCompat, ModelConfig};

let mut config = ModelConfig::google("my-gemini-proxy-alias", "Gemini via proxy");
config.google = Some(GoogleCompat::force_thinking_level()); // or force_thinking_budget()
```

A forced `thinkingLevel` on an unrecognised id clamps `Minimal` / `Off` to
`LOW`, since the crate cannot know whether that model accepts `MINIMAL`.

### Streaming

Uses SSE format (`alt=sse`). Each chunk contains `candidates` with `content.parts` and optional `usageMetadata`.

## Google Vertex AI

`GoogleVertexProvider` uses the same message format but with Vertex AI authentication and endpoints.

- **Protocol**: `ApiProtocol::GoogleVertex`
- **Auth**: OAuth2 / service account credentials
- **Endpoint pattern**: `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:streamGenerateContent`
