//! Runs on `wasm32-unknown-unknown` under Node (wasm-bindgen-test), against a
//! fake `env.AI`: a JavaScript object whose `run(model, input)` records its
//! arguments and returns what Workers AI would.
//!
//! ```bash
//! CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
//!   cargo test --target wasm32-unknown-unknown
//! ```
#![cfg(target_arch = "wasm32")]

use js_sys::{Array, Function, Object, Reflect};
use serde_json::{json, Value};
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;
use yoagent::decision::{DecisionError, DecisionModel, Question};
use yoagent_workers::ai::{self, AiBackend, CLEF};

/// A fake binding whose `run` executes `body` (JavaScript, with `model` and
/// `input` in scope) after recording its arguments in `this.calls`.
fn fake_ai(body: &str) -> Object {
    let ai = Object::new();
    Reflect::set(&ai, &"calls".into(), &Array::new()).unwrap();
    let run = Function::new_with_args(
        "model, input",
        &format!("this.calls.push({{ model, input }}); {body}"),
    );
    Reflect::set(&ai, &"run".into(), &run).unwrap();
    ai
}

/// A fake binding that resolves to `output`.
fn answering(output: &Value) -> Object {
    fake_ai(&format!("return Promise.resolve({output});"))
}

/// What `run` was called with, as JSON.
fn calls(ai: &Object) -> Vec<Value> {
    let calls = Reflect::get(ai, &"calls".into()).unwrap();
    let text = js_sys::JSON::stringify(&calls)
        .unwrap()
        .as_string()
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn clef_output() -> Value {
    json!({
        "model": "clef",
        "answers": {
            "urgent": {"type": "noul", "noul": 0.93},
            "team": {
                "type": "choice",
                "choice": "technical",
                "probabilities": {"billing": 0.04, "technical": 0.9, "sales": 0.06},
                "confidence": 0.85
            }
        },
        "usage": {"input_tokens": 2_000_000, "output_tokens": 0}
    })
}

#[wasm_bindgen_test]
async fn clef_runs_through_the_binding_and_is_priced() {
    let binding = answering(&clef_output());
    let clef = ai::clef(JsValue::from(binding.clone()));
    let eval = clef
        .ask("Checkout has been failing for every customer for the last hour.")
        .noul("urgent", "Is this support request urgent?")
        .question(
            "team",
            Question::choice_with_criteria(
                "Which team should handle this request?",
                [
                    ("billing", "Payments, invoices, and refunds"),
                    ("technical", "Outages, errors, and configuration"),
                    ("sales", "Plans and upgrades"),
                ],
            ),
        )
        .send()
        .await
        .unwrap();

    assert!((eval.noul("urgent").unwrap().p_true() - 0.93).abs() < 1e-9);
    assert_eq!(eval.choice("team").unwrap().choice(), "technical");
    // 2M input tokens at $0.24 per million.
    let cost = eval.cost_usd().unwrap();
    assert!((cost - 0.48).abs() < 1e-9, "{cost}");

    // One call: the catalog path, and the SystemOne request as a JS object.
    let calls = calls(&binding);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["model"], CLEF);
    assert_eq!(calls[0]["input"]["model"], "clef");
    assert_eq!(
        calls[0]["input"]["state"],
        "Checkout has been failing for every customer for the last hour."
    );
    assert_eq!(calls[0]["input"]["questions"]["urgent"]["type"], "noul");
    assert_eq!(
        calls[0]["input"]["questions"]["team"]["criteria"]["sales"],
        "Plans and upgrades"
    );
}

#[wasm_bindgen_test]
async fn clef_flash_asks_for_its_own_model() {
    let binding = answering(&json!({
        "model": "clef-flash",
        "answers": {"q": {"type": "noul", "noul": 0.2}},
        "usage": {"input_tokens": 1_000_000, "output_tokens": 0}
    }));
    let flash = ai::clef_flash(JsValue::from(binding.clone()));
    let eval = flash.ask("s").noul("q", "Is it?").send().await.unwrap();
    assert!((eval.cost_usd().unwrap() - 0.09).abs() < 1e-9);
    let calls = calls(&binding);
    assert_eq!(calls[0]["model"], "@cf/cloudflare/clef-flash");
    assert_eq!(calls[0]["input"]["model"], "clef-flash");
}

#[wasm_bindgen_test]
async fn a_rejected_run_is_a_backend_error_with_its_message() {
    let binding = fake_ai(
        "return Promise.reject(new Error('3040: Capacity temporarily exceeded, please try again.'));",
    );
    let err = ai::clef(JsValue::from(binding))
        .noul("s", "Is it?")
        .await
        .unwrap_err();
    match err {
        DecisionError::Backend { message, .. } => {
            assert!(message.contains("3040: Capacity"), "{message}")
        }
        other => panic!("expected Backend, got {other:?}"),
    }
}

#[wasm_bindgen_test]
async fn a_run_that_throws_before_its_promise_is_a_backend_error() {
    let binding = fake_ai("throw new Error('AiError: bad input');");
    let err = ai::clef(JsValue::from(binding))
        .noul("s", "Is it?")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, DecisionError::Backend { message, .. } if message.contains("bad input")),
        "{err:?}"
    );
}

#[wasm_bindgen_test]
async fn a_value_without_run_is_rejected_before_calling() {
    let not_a_binding = Object::new();
    let err = ai::clef(JsValue::from(not_a_binding))
        .noul("s", "Is it?")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, DecisionError::Invalid(m) if m.contains("`run`")),
        "{err:?}"
    );
}

#[wasm_bindgen_test]
async fn an_output_without_answers_is_a_bad_response() {
    let binding = fake_ai("return Promise.resolve(undefined);");
    let err = ai::clef(JsValue::from(binding))
        .noul("s", "Is it?")
        .await
        .unwrap_err();
    assert!(matches!(err, DecisionError::BadResponse(_)), "{err:?}");
}

#[wasm_bindgen_test]
async fn the_backend_plugs_into_any_decision_model() {
    // The raw backend, e.g. behind a fallback or with a custom price.
    let binding = answering(&clef_output());
    let model = DecisionModel::from_backend(AiBackend::new(JsValue::from(binding), CLEF), "clef");
    let eval = model.ask("s").noul("urgent", "Urgent?").send().await;
    // The fake answers a question this request did not ask ("team") as well;
    // answers nobody asked for are dropped.
    let eval = eval.unwrap();
    assert!(eval.choice("team").is_none());
    assert_eq!(eval.cost_usd(), None, "from_backend is unpriced");
}
