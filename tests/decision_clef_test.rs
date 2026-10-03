//! Clef on Cloudflare Workers AI, offline: the request yoagent sends to the
//! model's `/ai/run/` URL, Cloudflare's `{"result": ...}` envelope, its error
//! bodies, and the token variables.
//!
//! Its own binary because two tests set `CLOUDFLARE_API_TOKEN` /
//! `CLOUDFLARE_AUTH_TOKEN`; they hold `ENV` so they never interleave.

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request as WireRequest, ResponseTemplate};
use yoagent::decision::*;
use yoagent::retry::RetryConfig;

const RUN_PATH: &str = "/client/v4/accounts/acct-123/ai/run/@cf/cloudflare/clef";

static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn backend(server: &MockServer) -> SystemOneBackend {
    SystemOneBackend::workers_ai("acct-123", "@cf/cloudflare/clef")
        .with_endpoint_url(format!("{}{RUN_PATH}", server.uri()))
        .with_retry(RetryConfig::none())
}

/// The answers of Cloudflare's own example, in its output schema.
fn answers() -> Value {
    json!({
        "urgent": {"type": "noul", "noul": 0.93},
        "team": {
            "type": "choice",
            "choice": "technical",
            "probabilities": {"billing": 0.04, "technical": 0.9, "sales": 0.06},
            "confidence": 0.85
        },
        "severity": {
            "type": "score",
            "score": 2.6,
            "legend": {"0": "No impact", "1": "Minor", "2": "Major", "3": "Critical"},
            "probabilities": {"0": 0.0, "1": 0.05, "2": 0.3, "3": 0.65},
            "confidence": 0.5
        }
    })
}

fn enveloped() -> Value {
    json!({
        "result": {
            "model": "clef",
            "answers": answers(),
            "usage": {"input_tokens": 412, "output_tokens": 0}
        },
        "success": true,
        "errors": [],
        "messages": []
    })
}

#[tokio::test]
async fn sends_cloudflares_example_and_reads_the_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(RUN_PATH))
        .and(header("authorization", "Bearer cf-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(enveloped()))
        .expect(1)
        .mount(&server)
        .await;

    let model = DecisionModel::from_backend(backend(&server).with_api_key("cf-token"), "clef");
    let eval = model
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
        .score(
            "severity",
            "How severe is the customer impact?",
            ["No impact", "Minor", "Major", "Critical"],
        )
        .send()
        .await
        .unwrap();

    assert_eq!(eval.model(), "clef");
    assert_eq!(eval.usage().input_tokens, 412);
    assert!((eval.noul("urgent").unwrap().p_true() - 0.93).abs() < 1e-9);
    assert_eq!(eval.choice("team").unwrap().choice(), "technical");
    assert!((eval.score("severity").unwrap().score() - 2.6).abs() < 1e-9);

    // The body is Cloudflare's documented request: the model selector, the
    // state, and the questions keyed by id.
    let sent: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(sent["model"], "clef");
    assert_eq!(
        sent["state"],
        "Checkout has been failing for every customer for the last hour."
    );
    assert_eq!(sent["questions"]["urgent"]["type"], "noul");
    assert_eq!(
        sent["questions"]["team"]["criteria"]["technical"],
        "Outages, errors, and configuration"
    );
    assert_eq!(sent["questions"]["severity"]["criteria"][3], "Critical");
}

#[tokio::test]
async fn a_2xx_schema_failure_is_an_invalid_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "success": false,
            "errors": [{"code": 5006, "message": "Error: required properties at '/' are 'questions'"}],
            "messages": []
        })))
        .mount(&server)
        .await;
    let model = DecisionModel::from_backend(backend(&server).with_api_key("t"), "clef");
    match model.noul("state", "Is it?").await {
        Err(DecisionError::Invalid(m)) => assert!(m.contains("required properties"), "{m}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[tokio::test]
async fn an_http_error_keeps_cloudflares_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "result": null,
            "success": false,
            "errors": [{"code": 7003, "message": "Could not route to /accounts/acct-123/ai/run"}],
            "messages": []
        })))
        .mount(&server)
        .await;
    let model = DecisionModel::from_backend(backend(&server).with_api_key("t"), "clef");
    let err = model.noul("state", "Is it?").await.unwrap_err();
    assert!(err.to_string().contains("Could not route"), "{err}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn the_token_comes_from_either_variable_api_token_first() {
    let _env = ENV.lock().await;
    let server = MockServer::start().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    Mock::given(method("POST"))
        .respond_with(move |req: &WireRequest| {
            let auth = req
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            sink.lock().unwrap().push(auth);
            ResponseTemplate::new(200).set_body_json(json!({
                "result": {
                    "model": "clef",
                    "answers": {"q": {"type": "noul", "noul": 0.5}},
                    "usage": {"input_tokens": 1, "output_tokens": 0}
                },
                "success": true
            }))
        })
        .mount(&server)
        .await;
    let model = DecisionModel::from_backend(backend(&server), "clef");

    std::env::remove_var("CLOUDFLARE_API_TOKEN");
    std::env::set_var("CLOUDFLARE_AUTH_TOKEN", "auth-token");
    model.noul("s", "q").await.unwrap();
    // Both set: the wrangler name wins. Read per call, so no rebuild.
    std::env::set_var("CLOUDFLARE_API_TOKEN", "api-token");
    model.noul("s", "q").await.unwrap();
    std::env::remove_var("CLOUDFLARE_API_TOKEN");
    std::env::remove_var("CLOUDFLARE_AUTH_TOKEN");

    assert_eq!(
        *seen.lock().unwrap(),
        ["Bearer auth-token", "Bearer api-token"]
    );
}

#[tokio::test]
async fn no_token_fails_before_sending_and_names_both_variables() {
    let _env = ENV.lock().await;
    std::env::remove_var("CLOUDFLARE_API_TOKEN");
    std::env::remove_var("CLOUDFLARE_AUTH_TOKEN");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let model = DecisionModel::from_backend(backend(&server), "clef");
    match model.noul("s", "q").await {
        Err(DecisionError::MissingApiKey(vars)) => {
            assert_eq!(vars, "CLOUDFLARE_API_TOKEN or CLOUDFLARE_AUTH_TOKEN")
        }
        other => panic!("expected MissingApiKey, got {other:?}"),
    }
}

#[test]
fn presets_ask_for_clef_and_clef_flash() {
    assert_eq!(DecisionModel::clef("a").model(), "clef");
    assert_eq!(DecisionModel::clef_flash("a").model(), "clef-flash");
}

fn cloudflare_429(code: u64, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(429).set_body_json(json!({
        "result": null,
        "success": false,
        "errors": [{"code": code, "message": message}],
        "messages": []
    }))
}

#[tokio::test]
async fn a_used_up_daily_allocation_is_reported_once_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(cloudflare_429(
            3036,
            "You have used up your daily free allocation of 10,000 neurons.",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let retrying = RetryConfig {
        max_retries: 3,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 1,
    };
    let model = DecisionModel::from_backend(
        backend(&server).with_api_key("t").with_retry(retrying),
        "clef",
    );
    let err = model.noul("s", "Is it?").await.unwrap_err();
    assert!(!err.is_retryable(), "{err:?}");
    assert!(err.to_string().contains("daily free allocation"), "{err}");
}

#[tokio::test]
async fn out_of_capacity_is_retried_and_keeps_cloudflares_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(cloudflare_429(
            3040,
            "Capacity temporarily exceeded, please try again.",
        ))
        .expect(3)
        .mount(&server)
        .await;
    let retrying = RetryConfig {
        max_retries: 2,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 1,
    };
    let model = DecisionModel::from_backend(
        backend(&server).with_api_key("t").with_retry(retrying),
        "clef",
    );
    let err = model.noul("s", "Is it?").await.unwrap_err();
    assert!(
        matches!(err, DecisionError::RateLimited { status: 429, .. }),
        "{err:?}"
    );
    assert!(
        err.to_string().contains("Capacity temporarily exceeded"),
        "{err}"
    );
}

/// A server answering every request with `answers`, recording the
/// `authorization` header it received.
async fn recording_server() -> (MockServer, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let server = MockServer::start().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    Mock::given(method("POST"))
        .respond_with(move |req: &WireRequest| {
            let auth = req
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            sink.lock().unwrap().push(auth);
            ResponseTemplate::new(200).set_body_json(json!({
                "result": {
                    "model": "clef",
                    "answers": {"q": {"type": "noul", "noul": 0.5}},
                    "usage": {"input_tokens": 1, "output_tokens": 0}
                },
                "success": true
            }))
        })
        .mount(&server)
        .await;
    (server, seen)
}

#[tokio::test]
async fn an_empty_api_token_falls_through_and_values_are_trimmed() {
    let _env = ENV.lock().await;
    let (server, seen) = recording_server().await;
    let model = DecisionModel::from_backend(backend(&server), "clef");

    // CI systems export unset secrets as empty strings.
    std::env::set_var("CLOUDFLARE_API_TOKEN", "  ");
    std::env::set_var("CLOUDFLARE_AUTH_TOKEN", "auth-token\n");
    let result = model.noul("s", "q").await;
    std::env::remove_var("CLOUDFLARE_API_TOKEN");
    std::env::remove_var("CLOUDFLARE_AUTH_TOKEN");
    result.unwrap();
    assert_eq!(*seen.lock().unwrap(), ["Bearer auth-token"]);
}

#[tokio::test]
async fn variables_set_but_empty_are_called_that() {
    let _env = ENV.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let model = DecisionModel::from_backend(backend(&server), "clef");
    std::env::set_var("CLOUDFLARE_API_TOKEN", "");
    std::env::remove_var("CLOUDFLARE_AUTH_TOKEN");
    let result = model.noul("s", "q").await;
    std::env::remove_var("CLOUDFLARE_API_TOKEN");
    match result {
        Err(DecisionError::MissingApiKey(vars)) => assert_eq!(
            vars,
            "CLOUDFLARE_API_TOKEN or CLOUDFLARE_AUTH_TOKEN (set, but empty)"
        ),
        other => panic!("expected MissingApiKey, got {other:?}"),
    }
}

#[tokio::test]
async fn a_rejected_token_names_its_variable() {
    let _env = ENV.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "success": false,
            "errors": [{"code": 10000, "message": "Authentication error"}]
        })))
        .mount(&server)
        .await;
    let model = DecisionModel::from_backend(backend(&server), "clef");
    std::env::set_var("CLOUDFLARE_API_TOKEN", "wrong-scope");
    let result = model.noul("s", "q").await;
    std::env::remove_var("CLOUDFLARE_API_TOKEN");
    let err = result.unwrap_err();
    assert!(
        matches!(err, DecisionError::Http { status: 403, .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("$CLOUDFLARE_API_TOKEN"), "{err}");
    assert!(
        !err.to_string().contains("wrong-scope"),
        "the key is never shown"
    );
}
