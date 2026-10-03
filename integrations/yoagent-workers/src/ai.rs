//! The Workers AI binding (`env.AI`) as a decision backend.
//!
//! [`AiBackend`] calls `env.AI.run(model, input)` with the SystemOne request
//! yoagent builds and parses the result with
//! [`parse_systemone_response`], so answers are validated, priced and
//! recorded exactly as they are for the REST preset
//! ([`DecisionModel::clef`]). [`clef`] and [`clef_flash`] are the ready-made
//! models.

use async_trait::async_trait;
use js_sys::{Function, Promise, Reflect, JSON};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use yoagent::decision::{
    parse_systemone_response, Capabilities, DecisionBackend, DecisionError, DecisionModel,
    Evaluation, QuestionKind, Request,
};

/// Clef (27B) in the Workers AI catalog.
pub const CLEF: &str = "@cf/cloudflare/clef";
/// Clef Flash (9B, faster) in the Workers AI catalog.
pub const CLEF_FLASH: &str = "@cf/cloudflare/clef-flash";

/// The `prices.json` provider key for Workers AI's models.
const PRICE_PROVIDER: &str = "cloudflare";

/// A SystemOne decision model run through a Worker's AI binding.
///
/// Holds the binding object (`env.AI`) and the catalog path of the model to
/// run. Errors the binding throws (capacity, limits, a bad request) come back
/// as [`DecisionError::Backend`] carrying the binding's message; they are not
/// retried, but a [`DecisionModel::or`] fallback still applies.
#[derive(Clone, Debug)]
pub struct AiBackend {
    ai: JsValue,
    model_path: String,
    capabilities: Capabilities,
}

impl AiBackend {
    /// Run `model_path` (e.g. [`CLEF`]) through `ai`, the Worker's AI
    /// binding: workers-rs's `Ai` (`env.ai("AI")?`) or the raw `env.AI`
    /// `JsValue`.
    ///
    /// Capabilities: every question type, 255 options, 10 levels, batching,
    /// and SystemOne's 64k/32k token estimates.
    pub fn new(ai: impl Into<JsValue>, model_path: impl Into<String>) -> Self {
        Self {
            ai: ai.into(),
            model_path: model_path.into(),
            capabilities: Capabilities::new(QuestionKind::all())
                .with_token_limits(Some(64_000), Some(32_000)),
        }
    }

    /// Report these capabilities instead.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// The catalog path this backend runs.
    pub fn model_path(&self) -> &str {
        &self.model_path
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
            .call2(&self.ai, &JsValue::from_str(&self.model_path), &input)
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
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            DecisionError::BadResponse(format!("the AI binding's output is not JSON: {e}"))
        })?;
        parse_systemone_response(value, request)
    }
}

/// Clef through the Worker's AI binding: model `clef`, priced from yoagent's
/// price table (`cloudflare/clef`, $0.24 per million input tokens) as it
/// resolves now.
pub fn clef(ai: impl Into<JsValue>) -> DecisionModel {
    model(ai, CLEF, "clef")
}

/// Clef Flash through the Worker's AI binding: model `clef-flash`
/// (`cloudflare/clef-flash`, $0.09 per million input tokens).
pub fn clef_flash(ai: impl Into<JsValue>) -> DecisionModel {
    model(ai, CLEF_FLASH, "clef-flash")
}

fn model(ai: impl Into<JsValue>, path: &str, id: &str) -> DecisionModel {
    let cost = yoagent::provider::prices::global::resolved().cost(PRICE_PROVIDER, id);
    DecisionModel::from_backend(AiBackend::new(ai, path), id).with_cost(cost)
}

fn thrown(e: JsValue) -> DecisionError {
    DecisionError::backend(format!("Workers AI: {}", describe(&e)))
}

/// A JavaScript value as text: an `Error`'s message, a string, or its JSON.
fn describe(v: &JsValue) -> String {
    if let Some(e) = v.dyn_ref::<js_sys::Error>() {
        return String::from(e.message());
    }
    if let Some(s) = v.as_string() {
        return s;
    }
    JSON::stringify(v)
        .ok()
        .and_then(|s| s.as_string())
        .unwrap_or_else(|| format!("{v:?}"))
}
