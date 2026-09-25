# Model Pricing

yoagent reports what a run cost (`Agent::session_cost_usd`, `SessionStats`,
the `llm_stream` span's `cost_usd`) from the `CostConfig` on
`ModelConfig::cost`. This page covers where those rates come from.

## The data file

Every price the crate knows lives in one JSON file,
[`src/provider/prices.json`](https://github.com/yologdev/yoagent/blob/main/src/provider/prices.json),
embedded at compile time. It is keyed by `ModelConfig::provider`, then by
`ModelConfig::id`:

```json
{
  "schema": 1,
  "providers": {
    "anthropic": {
      "claude-opus-5-5": {
        "input": 4.0,
        "output": 20.0,
        "cache_read": 0.2,
        "cache_write": 5.0,
        "note": "Cache hits bill at 0.05x input ...",
        "source": "https://platform.claude.com/docs/en/about-claude/pricing",
        "verified": "2026-09-25"
      }
    },
    "openai": {
      "gpt-6-sol": {
        "input": 2.0,
        "output": 10.0,
        "cache_read": 0.2,
        "cache_write": 2.5,
        "tiers": [
          {
            "above_prompt_tokens": 272000,
            "input": 4.0,
            "output": 15.0,
            "cache_read": 0.4,
            "cache_write": 5.0
          }
        ],
        "source": "https://developers.openai.com/api/docs/pricing",
        "verified": "2026-09-25"
      }
    }
  }
}
```

| Field | Meaning |
|-------|---------|
| `schema` | Format version. This release reads `1` and rejects anything else. |
| `input`, `output` | USD per million tokens. Required. |
| `cache_read`, `cache_write` | USD per million cached / cache-written prompt tokens. Omitted or `0` bills at the band's input rate (see [the zero-rate rule](../reference/configuration.md#costconfig)). |
| `tiers` | Context tiers: above `above_prompt_tokens` prompt tokens the whole request bills at the tier's rates. Strictly ascending. |
| `cache_write_at_input` | The vendor has no cache-write premium and the entry states `cache_write` as the input rate. Validated: every band's `cache_write` must equal its `input`. |
| `note` | A caveat, e.g. a rate the vendor page does not state itself. |
| `source` | The vendor page the rates were checked against — the authority. |
| `verified` | The date (`YYYY-MM-DD`) they were checked. |
| `absent_upstream` | Why models.dev lacks this model, and when that was checked (read by the price audit). |

Parsing validates: rates must be finite and non-negative, tier thresholds
strictly ascending and above zero, and unknown fields are rejected — a
misspelled `"cache_reed"` is an error, not a silently unpriced cache.

In code the file is a `PriceTable`:

```rust
use yoagent::provider::PriceTable;

let builtin = PriceTable::builtin();
let opus = builtin.cost("anthropic", "claude-opus-5-5"); // Option<CostConfig>
for (provider, id, entry) in builtin.iter() {
    println!("{provider}/{id}: ${}/M in, verified {:?}", entry.cost.input_per_million, entry.verified);
}
let mine = PriceTable::from_path("my-prices.json")?; // or from_json_str
let combined = builtin.layered(&mine);               // mine wins per (provider, id)
println!("{}", combined.to_json());                   // back to the same format
```

## Which constructors are priced

Constructors look `(provider, id)` up **when the config is built**:

- **Named presets** (`claude_fable_5`, `claude_fable_5_1`, `claude_opus_5_5`,
  `claude_opus_5`, `claude_opus_4_8`, `claude_sonnet_5`, `claude_haiku_4_5`,
  `gpt_5_5`, `gpt_6_astra`, `gpt_6_sol`, `gpt_6_luna`) are always listed.
- **Generic first-party constructors** — `anthropic`, `openai`,
  `openai_responses`, `google`, `xai`, `groq`, `deepseek`, `mistral`, `zai`,
  `minimax`, `qwen`, `meta` — return `Some` when the table lists the id and
  `None` otherwise. `ModelConfig::anthropic("claude-sonnet-5", ..)` is priced
  exactly like `claude_sonnet_5()`; `meta` prices `muse-spark-1.1` and
  `muse-spark-1.2` and leaves any other id (the far cheaper contributor tier,
  say) unknown.
- **Gateways and custom endpoints** — `custom`, `openai_compat`, `local`,
  `ollama`, `opencode_zen`, `opencode_go`, `mock` — are always `None`, even
  for an id the table lists. A gateway's bill is not the vendor's list price.

`None` means **unknown**, never free. Set `config.cost` yourself for a model
the table does not list; a value you set after construction always wins.

## Overriding prices at runtime

A price change should not have to wait for a yoagent release. Constructors
read a process-wide **resolved table**, built in layers. Highest precedence
first:

| # | Layer | Set by |
|---|-------|--------|
| 1 | Explicit `config.cost` | assigning the field after construction — it is a plain field, so it always wins |
| 2 | User override | `PriceTable::install_override(table)`, or the file named by `YOAGENT_PRICES` |
| 3 | Built-in | `src/provider/prices.json`, compiled in |

Each layer replaces **whole entries** per `(provider, id)`. An override file
may be partial: it overrides exactly the models it lists, and an entry it
lists replaces the built-in one entirely (a rate it omits is not inherited —
a missing cache rate bills at the input rate).

```rust
use yoagent::provider::{ModelConfig, PriceTable};

// At startup, before building any config:
PriceTable::install_override(PriceTable::from_path("prices.override.json")?);

let config = ModelConfig::claude_sonnet_5();   // priced from the override
```

Or, with no code change:

```text
YOAGENT_PRICES=/etc/myapp/prices.json ./myapp
```

- **Constructors resolve when they run.** A config built before
  `install_override` keeps the price it was built with. Install overrides
  first, or re-price an existing config with
  `config.with_prices(&PriceTable::resolved())`.
- `YOAGENT_PRICES` is read **once**, the first time any constructor (or
  `PriceTable::resolved` / `install_override`) touches the table. A missing,
  unreadable or invalid file does not panic: it is logged with
  `tracing::warn!` and ignored, and the built-in prices apply.
- `install_override` **replaces** the user layer — including one loaded from
  `YOAGENT_PRICES` — rather than merging into it. `PriceTable::clear_override()`
  removes it. `PriceTable::resolved()` returns a snapshot of what a
  constructor would use now.
- Gateways and custom endpoints ignore every layer; see above.

## Re-pricing a config

`ModelConfig::with_prices(&table)` re-resolves one config against a table you
hold, without touching any global state. A model the table lists gets its
rates; one it does not list keeps its current cost:

```rust
let mine = PriceTable::from_json_str(r#"{"schema": 1, "providers": {
    "anthropic": {"claude-sonnet-5": {"input": 1.8, "output": 9.0,
                                      "cache_read": 0.18, "cache_write": 2.25}}}}"#)?;
let config = ModelConfig::claude_sonnet_5().with_prices(&mine); // your negotiated rate
```

It applies to gateways too: calling it is you saying what you pay there.

## Keeping the data honest

`tests/price_audit.rs` diffs **every** `prices.json` entry against
[models.dev](https://models.dev) — rates, tiers and thresholds — and fails on
drift, on an entry that vanished upstream, and on a comparison that silently
checked nothing. models.dev is community-maintained, so a failure sends a
human to the entry's `source`; the audit never edits the file. Run it before a
release:

```text
cargo test --all-features --test price_audit -- --ignored --nocapture
```
