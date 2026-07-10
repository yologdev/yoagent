# Structured Outputs

Get a **typed, schema-validated reply** instead of free text. The JSON Schema
is enforced natively by the provider — not by prompt begging.

```rust
use yoagent::{Agent, provider::ModelConfig};

#[derive(serde::Deserialize)]
struct Invoice {
    vendor: String,
    total_cents: u64,
    line_items: Vec<String>,
}

let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"));

let invoice: Invoice = agent
    .prompt_structured(
        "Extract the invoice from the attached text: ...",
        serde_json::json!({
            "type": "object",
            "properties": {
                "vendor": {"type": "string"},
                "total_cents": {"type": "integer"},
                "line_items": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["vendor", "total_cents", "line_items"]
        }),
    )
    .await?;
```

Derive the schema however you like — by hand as above, or with the
[`schemars`](https://crates.io/crates/schemars) crate
(`schemars::schema_for!(Invoice)`).

## How each provider enforces it

| Protocol | Mechanism |
|----------|-----------|
| Anthropic | Forced tool call — a synthetic tool is built from your schema and `tool_choice` forces it; the loop unwraps the call back into text |
| OpenAI-compatible | `response_format: {type: "json_schema", strict: true}` |
| Google Gemini | `generationConfig.responseSchema` + JSON mime type (note: Gemini uses an OpenAPI-style schema dialect — your schema is passed through as given) |
| OpenAI Responses / Azure / Vertex / Bedrock | Not yet wired — a warning is logged and the model replies as free text, which still must parse into `T` |

## Semantics & caveats

- `prompt_structured` runs the loop to completion internally and returns the
  parsed `T` — there is no event receiver for this call.
- On parse failure you get `StructuredPromptError::Parse { error, raw }` — the
  raw model text is preserved so you can retry or salvage.
- On Anthropic the forced tool call preempts regular tools for that request.
  Treat structured prompts as **extraction/finalization calls**, not agentic
  tool-using turns.
- Markdown code fences around the JSON are stripped defensively before
  parsing.
