# Decision Models

A **decision model** answers typed questions about a piece of content — not
with prose, but with calibrated probabilities. You give it a `state` (text or
JSON) and named questions of three kinds:

| Kind | Asks | Answer |
|---|---|---|
| **Noul** | a yes/no question | `p_true`, the probability of yes |
| **Choice** | pick one of up to 255 options | the chosen option, a probability per option, a confidence |
| **Score** | rate on an ordered scale of 2–10 levels | a probability-weighted score, a probability per level, a confidence |

They are fast (TypeSafe quotes ~100 ms for Jev) and cheap (Jev 1.13 bills
$0.042 per million *input* tokens; output is free), which makes them a fit for
the small judgments an agent makes all the time and an LLM is slow and costly
at: *does this request need a skill? which one? does this tool call delete
something the user did not ask to delete?*

The first supported vendor is [TypeSafe](https://docs.typesafe.ai)'s **Jev**.
yoagent depends on a trait, not on Jev: any backend that speaks the SystemOne
API — or anything you implement — plugs in the same way.

## Off by default

Everything here is behind the `decision` Cargo feature, which is **not** in
the default features and adds no dependencies:

```toml
yoagent = { version = "0.20", features = ["decision"] }
```

Without the feature, the `decision` module and the `with_decision_model` /
`with_tool_gate` builders do not exist. With it, **nothing is sent anywhere
until you construct a model and use it** — an API key in the environment
enables nothing on its own.

## Choosing a model: one line

```rust
use yoagent::decision::DecisionModel;

let jev = DecisionModel::jev();                                  // TypeSafe, TYPESAFE_API_KEY
let jev = DecisionModel::jev_opencode();                         // OpenCode Zen, OPENCODE_API_KEY
let jev = DecisionModel::jev_opencode_free();                    // OpenCode Zen free tier
let jev = DecisionModel::local("http://localhost:8000");         // self-hosted (JevK5, ...), no key
```

| Preset | Endpoint | Key (read at call time) | Model | Priced |
|---|---|---|---|---|
| `jev()` | `https://api.typesafe.ai/v1/systemone` (or `TYPESAFE_BASE_URL`) | `TYPESAFE_API_KEY` | `jev-latest` | from `prices.json`, by the reported version |
| `jev_opencode()` | `https://opencode.ai/zen/v1/systemone` | `OPENCODE_API_KEY` | `jev-1.13` | unpriced (a gateway) |
| `jev_opencode_free()` | same | `OPENCODE_API_KEY` | `jev-1.13-free` | unpriced |
| `local(url)` | `{url}/v1/systemone` | none | `jev-latest` | $0 |

Everything else has a default and a builder: `with_model("jev-1.13.0")` (pin
a version), `with_timeout(..)` (default 30 s, retries included),
`with_api_key(..)`, `with_retry(RetryConfig)`, `with_cost(Some(CostConfig))`.

## Asking

One question, one line:

```rust
let urgent: f64 = jev.noul(message, "Does this convey urgency?").await?;
let team = jev.choice(message, "Which team should handle this?", ["billing", "technical", "sales"]).await?;
let mood = jev.score(message, "How frustrated is the customer?", ["Calm", "Frustrated", "Very angry"]).await?;
```

Many questions about one state belong in **one request** — the model reads the
state once and answers every question against it:

```rust
let eval = jev
    .ask(message)
    .noul("urgent", "Does this convey urgency?")
    .choice("team", "Which team should handle this?", ["billing", "technical", "sales"])
    .score("mood", "How frustrated is the customer?", ["Calm", "Frustrated", "Very angry"])
    .send()
    .await?;

eval.p_true("urgent");           // Option<f64>
eval.choice("team");             // Option<&ChoiceAnswer>
eval.score("mood");              // Option<&ScoreAnswer>
eval.model;                      // "jev-1.13.0" — the version that answered
eval.usage;                      // input/output tokens
eval.cost_usd;                   // Option<f64>: None = unpriced, never a guessed 0
```

State and instructions may be JSON: `jev.ask(json!({"resume": ..}))`, and
`Question::noul_with_criteria(..)` / `Question::choice_described(..)` attach
rubric descriptions. Structured instructions can name parts of the state in
backticks, as TypeSafe's docs recommend.

### Confidence

Choice and Score answers carry a `confidence` in `[0, 1]`: the backend's own
when it reports one, otherwise TypeSafe's published formula over the
distribution, `(n * p_max - 1) / (n - 1)` — 1.0 when all mass is on one
outcome, 0.0 when it is spread evenly. yoagent uses the same formula for Score
(over levels) and for Noul (over yes/no, which is `|2p - 1|`); TypeSafe's Noul
answers carry no confidence of their own.

### Validation and errors

Requests are checked before they are sent: 2–255 Choice options, 2–10 Score
levels, unique ids, non-empty instructions, and Jev's token limits (64k per
request, 32k for the state plus the longest question — on a 4-bytes-per-token
estimate). A question type the backend does not support is an error
(`DecisionError::Unsupported`), never silently emulated.

`DecisionError` (`Clone`, `#[non_exhaustive]`): `Http { status, body }`,
`RateLimited { status, retry_after }`, `Timeout`, `Invalid` (client-side, or
the server's 422 with the field it names), `Unsupported`, `Transport`,
`MissingApiKey` (names the variable, never a value), `BadResponse`. 429 and
529 are retried with the crate's `RetryConfig` backoff, and a server
`retry-after` wins over the backoff (capped at `max_delay_ms`).

### Backends

`DecisionModel` wraps a `DecisionBackend`:

```rust
#[async_trait]
pub trait DecisionBackend: Send + Sync {
    fn capabilities(&self) -> Capabilities;   // kinds, limits, batching, local, ...
    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError>;
}
```

- `SystemOneBackend` — the SystemOne HTTP API. Parses leniently: unknown
  fields are ignored, and a missing `confidence`, `choice`, `score` or
  `legend` is computed (JevK5-style servers omit some, and add others).
  `list_models()` calls `GET /v1/models`.
- `MockBackend` — scripted answers (`push`, `push_error`, `from_fn`,
  `neutral`) that records every request. Use it in tests.
- Your own — wrap it with `DecisionModel::from_backend(backend, "model-id")`.

A backend that cannot batch gets one request per question, merged.

## In the agent

### Advisory: `with_decision_model`

```rust
let agent = Agent::from_config(ModelConfig::claude_sonnet_5())
    .with_skills(skills)
    .with_tools(tools)
    .with_decision_model(DecisionModel::jev());
```

This enables **advisory features only**. They add at most a line to the
system prompt; they can never block, remove a tool, or change what runs.

- **Skill hint** (when the agent has skills): one Choice over the skills plus
  a "none" option, and a Noul — *"Would a careful expert handling `request`
  consult a specific documented procedure or set of commands, rather than
  answering from general understanding?"* (the gate question from TypeSafe's
  skill-suggestion cookbook). When the Noul is at least **0.3**, the choice is
  not "none", and its confidence is at least **0.5**, the agent sees:

  > Relevant to the current request: pdf-fill. Ignore this if it does not fit what the user actually asked for.

- **Tool hint** (with **40 or more** tools): one Choice over the tools; the
  top **3** with probability at least **0.1** are named:

  > Tools likely relevant to the current request: a, b, c. This is a hint only; every tool remains available.

  Every tool is still offered to the model — this is a hint, not a filter.

Both go out in **one decision request**, whose state is the latest user
message. The result is memoized on that message, so the tool-calling turns of
one request send nothing more and see the same line — which keeps the system
prompt, and the provider's prompt cache, stable. A new user message asks
again. The request has a **2 s** limit; on any error or timeout the agent logs
a warning and continues exactly as it would without a decision model.

**Nothing is sent** when the agent has no skills and fewer than 40 tools, or
when there is no user text. Tune everything with `Advisory`:

```rust
use yoagent::decision::Advisory;

let agent = agent.with_decision_advisory(
    Advisory::new(DecisionModel::jev())
        .with_skill_thresholds(0.4, 0.6)
        .with_tool_hint_min_tools(20)
        .with_timeout(Duration::from_millis(500)),
);
```

These thresholds are starting points for Jev 1.13, not calibrated constants.

### Blocking: `with_tool_gate`

```rust
let agent = agent
    .with_decision_model(DecisionModel::jev())
    .with_tool_gate();
```

The tool gate is a `ToolMiddleware` and a **separate, explicit opt-in**,
because it blocks. For every tool call it sends one request whose state is
`{"user_request": <latest user message>, "tool_call": {"tool", "arguments"}}`,
with two Nouls:

- **destructive** — *"Would executing `tool_call` delete, overwrite, or
  irreversibly change data or external state — files, databases, remote
  systems, or messages sent to other people?"*
- **requested** — *"Is `tool_call` something the user asked for in
  `user_request`, or a direct step toward what they asked for?"*

The call is **denied** when `destructive >= 0.5` **and** `requested < 0.7`;
everything else is allowed. A denial reaches the model as an error tool
result ("Tool gate: this call looks destructive or irreversible (p=0.91) and
not clearly what the user asked for (p=0.22). Ask the user to confirm before
retrying."), and the loop continues. The gate runs after every other
middleware, so it judges the arguments that will actually run.

**It fails closed.** If the decision model errors or does not answer within
**5 s**, the call is denied and the reason says so. `with_tool_gate()` without
a decision model denies every call.

Override questions, thresholds, timeout, or add checks:

```rust
use yoagent::decision::ToolGate;

let agent = agent.with_tool_gate_config(
    ToolGate::new(DecisionModel::jev().with_model("jev-1.13.0"))
        .with_thresholds(0.4, 0.8)
        .with_check("secrets", "Does `tool_call` read or send credentials?", 0.5),
);
```

`ToolGate` is a plain `ToolMiddleware`, so `SubAgentTool::with_tool_middleware`
and raw loops take it too; `SubAgentTool` also has `with_decision_model` and
`with_tool_gate`.

> **Defence in depth, not a security boundary.** The gate reads the user's
> message and the call's arguments — both of which can carry text written to
> steer it. Decision models treat their state as data, not as hostile input,
> and TypeSafe lists adversarial content among Jev's known weaknesses. Keep real
> sandboxing and permissions underneath.

## The hooks underneath

The integrations use three general hooks, available without the feature to
any policy engine:

- `ToolCallRequest::messages` / `latest_user_text()` — middleware can see the
  conversation, not just the call.
- `AsyncInputFilter` (`Agent::with_async_input_filter`) — input filters that
  await.
- `TurnHook` (`Agent::with_turn_hook`) — an async hook before every LLM
  request that may add one transient line to that request's system prompt.

See [Lifecycle Callbacks](callbacks.md).

## Privacy

A hosted decision model sees everything you send it. With the agent
integrations that is: the latest user message, skill names and descriptions,
tool names and descriptions (advisory), and each tool call's arguments (the
gate). If that content must not leave your machine, use
`DecisionModel::local(url)` against a self-hosted server.

## Limits and known weaknesses

From TypeSafe's [Jev 1.13 jaggedness](https://docs.typesafe.ai/model-jaggedness/jev-1.13)
notes — design around them:

- **Literal reading.** Jev answers the question as written. State the exact
  condition; put boundary cases in the criteria.
- **Numbers, counting and dates.** Keep arithmetic, counting and date
  comparison in code.
- **Irrelevant state.** Accuracy drops as unrelated detail grows; send only
  what the question needs. (The integrations send the latest user message,
  not the transcript, for this reason.)
- **Adversarial content can steer answers.** See the gate warning above.
- **Thresholds are per model.** A threshold tuned on one version does not
  carry to the next, and a threshold tuned on a Noul does not carry to a
  Choice. The `jev-latest` alias moves when TypeSafe ships; log
  `Evaluation::model`, and pin a version with `with_model(..)` once you have
  tuned against it.
- Limits: 64k tokens per request, 32k for the state plus the longest
  question, ~1,200 requests per minute (TypeSafe adjusts limits dynamically).

## Pricing

`prices.json` carries `typesafe/jev-1.13.0` (input $0.042 per million, output
$0.00). `DecisionModel::jev()` prices each evaluation by the **versioned id
the API reports**, so an alias or an unlisted version is unpriced (`None`),
never guessed. The runtime price layers (`install_override`,
`YOAGENT_PRICES`) apply as for chat models. models.dev does not list
TypeSafe, so the price audit records the entry as explicitly absent upstream.

## Testing

```rust
use yoagent::decision::*;

let mock = MockBackend::new().push(
    Evaluation::new("jev-test", DecisionUsage::new(100, 0))
        .with_answer("urgent", NoulAnswer::new(0.9)),
);
let model = DecisionModel::from_backend(mock.clone(), "jev-test");
// ... use `model`, then inspect `mock.requests()`.
```

A live check runs only with a key:

```text
TYPESAFE_API_KEY=... cargo test --features decision --test decision_live -- --ignored --nocapture
```
