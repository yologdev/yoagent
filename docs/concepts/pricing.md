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

Parsing validates every entry, through the one choke point
`PriceTable::insert` (and `PriceEntry::validate`, which you can call
yourself):

- provider and model ids are non-empty;
- rates are finite and non-negative;
- tier thresholds are above zero and strictly ascending;
- `verified` is a `YYYY-MM-DD` date;
- a `cache_write_at_input` flag agrees with the rates.

**Unknown fields** depend on who maintains the data:

- **Strict**, for data people maintain by hand: `PriceTable::from_json_str`,
  `from_path`, the `YOAGENT_PRICES` file and `PriceSource::Url`. An unknown
  field is `PriceError::UnknownField`, so a misspelled `"cache_reed"` is an
  error, not a silently unpriced cache. It may also mean the data was written
  for a newer yoagent; the error says to upgrade or remove the field.
- **Lenient**, for this crate's own published file (`PriceSource::YoagentMain`)
  and the `fetch_cached` cache: unknown fields are ignored, logged at `warn`
  and returned to you (`FetchReport::ignored_fields`,
  `CachedPrices::ignored_fields`).

### Format evolution

- A field that **changes what is billed** (a new rate, a new kind of tier)
  **bumps `schema`**. An older yoagent then rejects the file with
  `PriceError::UnsupportedSchema`, rather than billing without the field.
- A **metadata-only** field (like `note`) does **not** bump `schema`. Older
  releases ignore it in this crate's published file.

A test pins the set of field names to the schema version. Changing the set
fails CI until someone decides which of the two cases it is.

In code the file is a `PriceTable`, plain data:

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

`PriceError` is `Clone` and `#[non_exhaustive]`. `Json`, `Io` and `Http`
errors name the file or URL they came from.

## Which constructors are priced

Constructors look `(provider, id)` up **when the config is built**:

- **Named presets** (`claude_fable_5`, `claude_fable_5_1`, `claude_opus_5_5`,
  `claude_opus_5`, `claude_opus_4_8`, `claude_sonnet_5`, `claude_haiku_4_5`,
  `gpt_5_5`, `gpt_6_astra`, `gpt_6_sol`, `gpt_6_luna`) are always listed.
- **Generic first-party constructors** — those whose provider is in
  `PRICED_PROVIDERS`: `anthropic`, `openai` and `openai_responses` (both use
  provider `openai`), `google`, `xai`, `groq`, `deepseek`, `mistral`, `zai`,
  `minimax`, `qwen`, `meta` — return `Some` when the table lists the id and
  `None` otherwise. `ModelConfig::anthropic("claude-sonnet-5", ..)` is priced
  exactly like `claude_sonnet_5()`; `meta` prices `muse-spark-1.1` and
  `muse-spark-1.2` and leaves any other id (the far cheaper contributor tier,
  say) unknown.
- **Gateways and custom endpoints** — `custom`, `openai_compat`, `local`,
  `ollama`, `opencode_zen`, `opencode_go`, `mock` — are always `None`, even
  for an id the table lists. A gateway's bill is not the vendor's list price.

`None` means **unknown**, never free. Set `config.cost` yourself for a model
the table does not list.

## Overriding prices at runtime

A price change should not have to wait for a yoagent release. Constructors
read a process-wide **resolved table**, managed by the
`yoagent::provider::prices::global` module. It is built in layers, highest
precedence first:

| # | Layer | Set by |
|---|-------|--------|
| 1 | User override | `global::install_override(table)`, or the file named by `YOAGENT_PRICES` |
| 2 | Fetched | `global::install_fetched(table)` — opt-in, see [Live sources](#live-sources) |
| 3 | Built-in | `src/provider/prices.json`, compiled in |

A `cost` you set on a config yourself wins over every layer, because only
constructors read them. But `config.reprice()` and `config.with_prices()`
overwrite it (see [Re-pricing a config](#re-pricing-a-config)).

Each layer replaces **whole entries** per `(provider, id)`. An override file
may be partial: it overrides exactly the models it lists. An entry it lists
replaces the lower entry entirely, so a rate it omits is not inherited — a
missing cache rate bills at the input rate.

```rust
use yoagent::provider::prices::global;
use yoagent::provider::{ModelConfig, PriceTable};

// At startup, before building any config:
let report = global::install_override(PriceTable::from_path("prices.override.json")?);
for c in &report.changes {
    tracing::info!("price override: {c}"); // e.g. "anthropic/claude-sonnet-5: input 2 -> 1.8, ..."
}

let config = ModelConfig::claude_sonnet_5();   // priced from the override
```

Or, with no code change:

```text
YOAGENT_PRICES=/etc/myapp/prices.json ./myapp
```

### What `install_override` reports

The whole install holds one lock and returns an `OverrideReport`
(`#[must_use]`), computed by comparing the resolved table before and after:

- `changes`: the models the new override prices differently. Entries no
  constructor reads are excluded.
- `reverted`: models whose price changed because a previous user layer listed
  them and this one does not. They revert to the fetched or built-in price,
  or become unpriced.
- `inert`: entries whose provider is not in `PRICED_PROVIDERS`. No
  constructor looks them up; only `with_prices` reads them. A gateway name
  like `openrouter` or `opencode-zen` is the typical case.
- `warnings`: everything below, also logged at `warn`.

An entry replaces the lower entry whole, and the mistakes that makes easy
are warned about:

- **Inert entries**, as above.
- **Dropped tiers.** The entry has no context tiers, but the entry it
  replaces — built-in or fetched — had some. Every request then bills at the
  base rates.
- **Unset cache rate.** The entry leaves a cache rate unset, so it bills at
  the input rate, where the replaced entry set one.
- **Replacing a user layer**, especially one loaded from `YOAGENT_PRICES`.
  `install_override` replaces the layer; it never merges into it.

`global::clear_override()` removes the user layer. `global::resolved()`
returns a snapshot of what a constructor would use now.

### `YOAGENT_PRICES`

- It is read **once**, the first time anything touches the process-wide
  table. Unset or empty means no file.
- A missing, unreadable or invalid file does not panic. It is logged with
  `tracing::warn!` and ignored, and no user layer is installed.
- The outcome is visible to the host, not just in the log:
  `global::env_override_status()` returns an `EnvOverride`:
  - `Unset`: the variable was unset or empty;
  - `Loaded { path, entries, warnings }`: the file loaded, with the same
    warnings `install_override` reports;
  - `Rejected { path, error }`: the file was ignored.

  A host that would rather fail than run on prices it did not ask for can
  check this at startup. `global::load_env_override()` reads and strictly
  parses the file right away and returns
  `Result<Option<PriceTable>, PriceError>`.
- This crate's own unit tests never read `YOAGENT_PRICES`. Integration tests
  and doctests that assert list prices call `global::clear_override()`
  first, and the suite passes with the variable set.

### Constructors resolve when they run

A config built before an install keeps the price it was built with. Install
prices first and build configs afterwards, or re-price what you already
hold: `config.reprice()`, `agent.reprice()`, or `SubAgentTool::reprice()`
(builder-style, before the tool is registered).

## Live sources

yoagent **never fetches prices on its own**. When you ask,
`PriceTable::fetch(&source)` downloads a table from a `PriceSource`:

| Source | What it is | Parsed |
|--------|------------|--------|
| `PriceSource::YoagentMain` | This crate's own `src/provider/prices.json` on GitHub `main`. Same format, same review, same audit as a release — a checked price fix merged to `main` reaches you without waiting for one. | leniently |
| `PriceSource::ModelsDev` | [models.dev](https://models.dev/api.json), mapped into this format. Covers thousands of models. | mapped |
| `PriceSource::ModelsDevAt(url)` | A models.dev-format document elsewhere (a mirror, a pinned snapshot). | mapped |
| `PriceSource::Url(url)` | Any URL serving this crate's format — typically a file you maintain. | strictly |

`PriceTable::fetch_with(&source, FetchOptions)` takes a timeout (default
10 s: `FetchOptions::new().with_timeout(..)`) and returns a `FetchReport`:

- `table`: the fetched prices;
- `skipped`: every model a models.dev source listed but could not map, and
  why (`SkippedModel`);
- `ignored_fields`: fields a lenient parse ignored.

`PriceTable::from_models_dev_json_report` returns the same report for a
document you already have.

### Caching

`PriceTable::fetch_cached(&source, path, CacheOptions)` wraps a fetch in a
cache file. It never fails. `CacheOptions::new()` defaults to refetching
after a day (`max_age`), accepting a stale cache at most a week old when
offline (`max_stale`), and a 10 s timeout. Each has a `with_*` setter.

```rust
use yoagent::provider::prices::global;
use yoagent::provider::{CacheOptions, PriceSource, PriceTable};

let prices = PriceTable::fetch_cached(
    &PriceSource::YoagentMain,
    cache_dir.join("yoagent-prices.json"),
    CacheOptions::new(),
)
.await;
if let Some(e) = prices.origin.fetch_error() {
    tracing::warn!("price refresh failed ({e}); using {:?}", prices.origin);
}
if !prices.origin.is_builtin() {
    let changes = global::install_fetched(prices.table);
    for c in changes.iter().filter(|c| c.before.is_some()) {
        tracing::info!("price changed: {c}");
    }
}
// ...then build configs.
```

The result is a `CachedPrices`. Its `origin` says where the table came from
and carries only the data that makes sense for that origin:

1. **`Cache { age }`.** A cache of this source younger than `max_age` was
   used without a request.
2. **`Fetched { cache_write_error }`.** The source was fetched and the cache
   rewritten, in this crate's format, recording the source URL. If the write
   failed, the table is still good and the error is here.
3. **`StaleCache { age, fetch_error }`.** The fetch failed, so an expired
   cache no older than `max_stale` was used.
4. **`Builtin { fetch_error }`.** The fetch failed and no cache was usable:
   the built-in data. Installing it changes nothing.

`CachedPrices` also carries `skipped` and `ignored_fields` (as in
`FetchReport`) and `cache_problem`: a cache file that existed but could not
be used. A `CacheProblem` is one of:

- `Unreadable`: the file could not be read;
- `Invalid`: the file is not a valid price cache;
- `SourceMismatch { cached, expected }`: the file caches another source.

Every cache problem is also logged at `warn`; only a *missing* file is
silent.

Some edge cases:

- **Unknown age.** A cache whose modification time is in the future, or
  unavailable, is never fresh. It serves as a stale fallback only when
  `max_stale` is `Duration::MAX`.
- **One path per source.** A cache of a different source is a miss, not a
  hit, so sharing a path just refetches.

### Installing: replace or only add

`global::install_fetched(table)` is
`global::install_fetched_with(table, InstallPolicy::ReplaceAll)`. It replaces
the fetched layer, so the table overrides the built-in data for every model
it lists. Replacing a non-empty fetched layer is logged at `warn`.

`InstallPolicy::AddOnly` merges into the current fetched layer only the
models that no lower layer — the built-in data or the current fetched layer —
lists. A fetched table can then extend coverage (models.dev's thousands of
models) without overriding any price already in effect.

Both are `#[must_use]`. They return `Vec<PriceChange>`, computed against the
built-in plus fetched table **before** the call. So a model the previous
fetched layer listed and the new one does not shows up too, with `after:
None` or its built-in price. A change the user layer overrides is still
reported, marked `shadowed: true`: it does not change what is billed until
the override is cleared.

`global::clear_fetched()` removes the fetched layer.

### Trust

With the default policy, a fetched table **overrides the built-in data** for
every model it lists.

- **`YoagentMain`** is as trustworthy as a release — it is the file releases
  are cut from. Suppose `main` moves to a newer schema than your yoagent
  reads, because a field that changes billing was added. The fetch then
  fails with `PriceError::UnsupportedSchema`, and nothing changes.
  Metadata-only fields do not bump the schema, and this source is parsed
  leniently, so older releases keep reading it (see
  [Format evolution](#format-evolution)).
- **`ModelsDev` is community-maintained and not authoritative.** It has been
  provably wrong before — it mis-stated Claude context-tier data and DeepSeek
  V4 Pro's price. The mapping is conservative:
  - A model whose cost carries structure `CostConfig` cannot express is
    **skipped**, not approximated. That covers a separate reasoning rate, a
    non-context tier and an unknown key.
  - A skipped model that the built-in data lists is **named in a `warn`
    log**. It keeps its built-in price.
  - Audio rates are ignored.
  - Provider keys are models.dev's own, with two renames to this crate's
    names: `alibaba` becomes `qwen`, and `opencode` becomes `opencode-zen`.
  - A document from which nothing maps is an error, never an empty table.

Either way, disagreements are visible, not silent. `install_fetched`
compares the new layer with the built-in data **as billed**: a zero cache
rate counts as the input rate. It logs, at `warn`, how many built-in models
it prices differently, spelling out the first five and counting the rest:

```text
WARN yoagent prices: the fetched table disagrees with the built-in data on 7 model(s)
     and takes precedence for them: anthropic/claude-sonnet-5: input 2 -> 1.5, ...; and 2 more
```

`fetched.changes_from(&PriceTable::builtin())` computes that list without
installing anything. Use it to decide whether to install at all, or install
with `InstallPolicy::AddOnly`.

## Re-pricing a config

There are two ways to re-price a config you already built, and they differ
on purpose.

**`config.reprice()`** repeats the constructor's own lookup against the
process-wide table **now**. Use it for configs built before an override or a
fetched layer was installed. `Agent::reprice()` and `SubAgentTool::reprice()`
do the same for the config they hold.

- The result is exactly what the constructor would set today, **including
  `None`** when the model is no longer listed.
- **A `cost` you assigned yourself is replaced.**
- It acts only on configs built by a first-party constructor that looks
  prices up — the named presets and `anthropic`, `openai`,
  `openai_responses`, `google`, `xai`, `groq`, `deepseek`, `mistral`, `zai`,
  `minimax`, `qwen`, `meta` — and only while their `provider` is still in
  `PRICED_PROVIDERS`.
- Gateways, custom endpoints, configs deserialized from disk and configs
  whose `provider` you changed are returned unchanged.

```rust
let config = ModelConfig::claude_sonnet_5();  // built before the override
let _ = global::install_override(mine);
let config = config.reprice();                // now priced from the override
```

**`config.with_prices(&table)`** re-resolves one config against a table you
hold, without touching any global state.

- A model the table lists gets its rates, replacing any `cost` you set.
- A model it does not list **keeps its current cost**: `with_prices` never
  clears a price.
- It applies to **any** config, gateways and custom endpoints included. It
  looks them up under their own `provider`, such as `opencode-zen`: calling
  it is you saying what you pay there.

```rust
let mine = PriceTable::from_json_str(r#"{"schema": 1, "providers": {
    "anthropic": {"claude-sonnet-5": {"input": 1.8, "output": 9.0,
                                      "cache_read": 0.18, "cache_write": 2.25}}}}"#)?;
let config = ModelConfig::claude_sonnet_5().with_prices(&mine); // your negotiated rate
```

## Keeping the data honest

`tests/price_audit.rs` diffs **every** `prices.json` entry against
[models.dev](https://models.dev) — rates, tiers and thresholds — and fails on
drift, on an entry that vanished upstream, and on a comparison that silently
checked nothing. The comparison logic also runs offline in CI, against a
checked-in slice of models.dev. models.dev is community-maintained, so a
failure sends a human to the entry's `source`; the audit never edits the
file. Run the live audit before a release:

```text
cargo test --all-features --test price_audit -- --ignored --nocapture
```
