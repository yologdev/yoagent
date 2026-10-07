//! The Workers AI binding (`env.AI`) as a decision backend.
//!
//! [`AiBackend`] calls `env.AI.run(model, input)` with the SystemOne request
//! yoagent builds and parses the result with [`parse_systemone_response`],
//! so answers are validated and spend is recorded as for the REST preset
//! ([`DecisionModel::clef`]). [`clef`] and [`clef_flash`] are the ready-made
//! models; they are priced at the rate the price table holds when they are
//! built.

use async_trait::async_trait;
use js_sys::{Function, Promise, Reflect, JSON};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use yoagent::decision::{
    parse_systemone_response, Capabilities, DecisionBackend, DecisionError, DecisionModel,
    Evaluation, QuestionKind, Request,
};
use yoagent::retry::RetryConfig;

/// Clef (27B) in the Workers AI catalog.
pub const CLEF: &str = "@cf/cloudflare/clef";
/// Clef Flash (9B, faster) in the Workers AI catalog.
pub const CLEF_FLASH: &str = "@cf/cloudflare/clef-flash";

/// The `prices.json` provider key for Workers AI's models.
const PRICE_PROVIDER: &str = "cloudflare";

/// Workers AI's error codes, as documented for its REST API
/// (<https://developers.cloudflare.com/workers-ai/platform/errors/>). A
/// binding error is read for a leading `NNNN:` code; how the binding words
/// its errors is not documented, so anything else is a plain
/// [`DecisionError::Backend`].
const OUT_OF_CAPACITY: u32 = 3040;
const DAILY_LIMIT: u32 = 3036;

/// A SystemOne decision model run through a Worker's AI binding.
///
/// Holds the binding object (`env.AI`) and the catalog path of the model to
/// run. What the binding throws or rejects with is classified like the REST
/// API's errors:
///
/// - out of capacity (`3040`): [`DecisionError::RateLimited`], retried with
///   [`with_retry`](Self::with_retry)'s policy (default
///   [`RetryConfig::default`]);
/// - daily free allocation used up (`3036`): [`DecisionError::Http`] 429, not
///   retried — nothing succeeds before the reset;
/// - anything else: [`DecisionError::Backend`] with the binding's message,
///   not retried.
///
/// A [`DecisionModel::or`] fallback applies to all of them.
#[derive(Clone, Debug)]
pub struct AiBackend {
    ai: JsValue,
    model_path: String,
    capabilities: Capabilities,
    retry: RetryConfig,
}

impl AiBackend {
    /// Run `model_path` (e.g. [`CLEF`]) through `ai`, the Worker's AI
    /// binding: workers-rs's `Ai` (`env.ai("AI")?`) or the raw `env.AI`
    /// `JsValue`.
    ///
    /// Capabilities: every question type, 255 options, 10 levels, batching,
    /// and SystemOne's 64k/32k token estimates — a state over them fails
    /// before the binding is called.
    pub fn new(ai: impl Into<JsValue>, model_path: impl Into<String>) -> Self {
        Self {
            ai: ai.into(),
            model_path: model_path.into(),
            capabilities: Capabilities::new(QuestionKind::all())
                .with_token_limits(Some(64_000), Some(32_000)),
            retry: RetryConfig::default(),
        }
    }

    /// Report these capabilities instead.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Retry policy for out-of-capacity errors (default
    /// [`RetryConfig::default`]: 3 retries, 1 s initial, 2x, 30 s cap).
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// The catalog path this backend runs.
    pub fn model_path(&self) -> &str {
        &self.model_path
    }

    async fn run_once(&self, input: &JsValue) -> Result<serde_json::Value, DecisionError> {
        let run = Reflect::get(&self.ai, &JsValue::from_str("run"))
            .ok()
            .and_then(|f| f.dyn_into::<Function>().ok())
            .ok_or_else(|| {
                DecisionError::Invalid(
                    "the AI binding has no `run` method: pass the Worker's `env.AI`".into(),
                )
            })?;
        // `run` may throw before returning its promise, or reject it.
        let returned = run
            .call2(&self.ai, &JsValue::from_str(&self.model_path), input)
            .map_err(thrown)?;
        let output = JsFuture::from(Promise::resolve(&returned))
            .await
            .map_err(thrown)?;
        let text = JSON::stringify(&output)
            .ok()
            .and_then(|s| s.as_string())
            .ok_or_else(|| {
                DecisionError::BadResponse(format!(
                    "the AI binding returned no JSON value: {}",
                    describe(&output)
                ))
            })?;
        serde_json::from_str(&text).map_err(|e| {
            DecisionError::BadResponse(format!("the AI binding's output is not JSON: {e}"))
        })
    }
}

#[async_trait(?Send)]
impl DecisionBackend for AiBackend {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        let body = serde_json::to_string(request)
            .map_err(|e| DecisionError::Invalid(format!("request does not serialize: {e}")))?;
        let input = JSON::parse(&body).map_err(|e| {
            DecisionError::Invalid(format!("request is not valid JSON: {}", describe(&e)))
        })?;
        let mut attempt = 0usize;
        let value = loop {
            match self.run_once(&input).await {
                Err(e) if e.is_retryable() && attempt < self.retry.max_retries => {
                    attempt += 1;
                    yoagent::rt::sleep(self.retry.delay_for_attempt(attempt)).await;
                }
                other => break other?,
            }
        };
        parse_systemone_response(value, request)
    }
}

/// Clef through the Worker's AI binding: model `clef`, priced at the
/// `cloudflare/clef` rate yoagent's process-wide price table holds when this
/// is called — **unpriced unless the Worker opted in to prices first**
/// ([`enable_bundled`](yoagent::provider::prices::enable_bundled): $0.24 per
/// million input tokens). A later opt-in or
/// [`install_override`](yoagent::provider::prices::global::install_override)
/// does not reprice the returned model; with prices enabled but no rate in
/// the table it is unpriced (logged).
pub fn clef(ai: impl Into<JsValue>) -> DecisionModel {
    model(ai, CLEF, "clef")
}

/// Clef Flash through the Worker's AI binding: model `clef-flash`, priced
/// like [`clef`] at the `cloudflare/clef-flash` rate ($0.09 per million input
/// tokens in the bundled snapshot; unpriced without an opt-in).
pub fn clef_flash(ai: impl Into<JsValue>) -> DecisionModel {
    model(ai, CLEF_FLASH, "clef-flash")
}

fn model(ai: impl Into<JsValue>, path: &str, id: &str) -> DecisionModel {
    use yoagent::provider::prices::global;
    let cost = global::resolved().cost(PRICE_PROVIDER, id);
    // Unpriced is yoagent's default; a missing rate is news only once the
    // Worker opted in to prices.
    if cost.is_none() && global::pricing_enabled() {
        tracing::warn!("no price for {PRICE_PROVIDER}/{id} in the price table; {id} is unpriced");
    }
    DecisionModel::from_backend(AiBackend::new(ai, path), id).with_cost(cost)
}

/// A binding failure, classified by its leading Workers AI error code.
fn thrown(e: JsValue) -> DecisionError {
    let message = format!("Workers AI: {}", describe(&e));
    match leading_code(&message["Workers AI: ".len()..]) {
        Some(OUT_OF_CAPACITY) => DecisionError::rate_limited_with_body(429, None, message),
        Some(DAILY_LIMIT) => DecisionError::http(429, message),
        _ => DecisionError::backend(message),
    }
}

/// The `NNNN` of a message starting `NNNN:` (after an optional `Name: `).
fn leading_code(message: &str) -> Option<u32> {
    let rest = match message.split_once(": ") {
        Some((name, rest)) if !name.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => message,
    };
    let (code, _) = rest.split_once(':')?;
    (code.len() == 4).then(|| code.parse().ok()).flatten()
}

/// A JavaScript value as text: an `Error`'s name and message, a string, or
/// its JSON.
fn describe(v: &JsValue) -> String {
    if let Some(e) = v.dyn_ref::<js_sys::Error>() {
        let name = String::from(e.name());
        let message = String::from(e.message());
        return if name.is_empty() || name == "Error" {
            message
        } else {
            format!("{name}: {message}")
        };
    }
    if let Some(s) = v.as_string() {
        return s;
    }
    JSON::stringify(v)
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_else(|| format!("{v:?}"))
}
