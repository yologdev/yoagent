//! Decision models, offline: the SystemOne wire format (TypeSafe's shape and
//! a JevK5-style variant), client-side validation, retries, error mapping,
//! confidence, the batch builder, and pricing.

use serde_json::{json, Value};
use std::time::{Duration, Instant};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request as WireRequest, ResponseTemplate};
use yoagent::decision::*;
use yoagent::retry::RetryConfig;

const KEY: &str = "sk-test-secret-key";

fn fast_retry() -> RetryConfig {
    RetryConfig {
        max_retries: 3,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 50,
    }
}

/// A hosted-style model pointed at the mock server, priced like `jev()`.
fn hosted(server: &MockServer) -> DecisionModel {
    DecisionModel::from_systemone(
        SystemOneBackend::typesafe()
            .with_base_url(server.uri())
            .with_api_key(KEY)
            .with_retry(fast_retry()),
        "jev-latest",
    )
}

fn typesafe_response() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "urgent": {"type": "noul", "noul": 0.95},
            "team": {
                "type": "choice",
                "choice": "billing",
                "probabilities": {"billing": 0.88, "technical": 0.12, "sales": 0.0},
                "confidence": 0.81
            },
            "mood": {
                "type": "score",
                "score": 1.05,
                "legend": {"0": "Calm", "1": "Frustrated", "2": "Very angry"},
                "probabilities": {"0": 0.0, "1": 0.95, "2": 0.05},
                "confidence": 0.92
            }
        },
        "usage": {"input_tokens": 1_000_000, "output_tokens": 34}
    })
}

async fn mount_ok(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn batched(model: &DecisionModel) -> Ask<'_> {
    model
        .ask("Help! My payouts have been failing for 3 days.")
        .noul("urgent", "Does this convey urgency?")
        .choice(
            "team",
            "Which team should handle this?",
            ["billing", "technical", "sales"],
        )
        .score(
            "mood",
            "How frustrated is the customer?",
            ["Calm", "Frustrated", "Very angry"],
        )
}

#[tokio::test]
async fn typesafe_shape_round_trips_a_batched_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(typesafe_response()))
        .expect(1)
        .mount(&server)
        .await;

    let eval = batched(&hosted(&server)).send().await.unwrap();

    assert_eq!(eval.model, "jev-1.13.0");
    let urgent = eval.noul("urgent").unwrap();
    assert_eq!(urgent.p_true, 0.95);
    assert!((urgent.confidence - 0.9).abs() < 1e-9, "computed |2p-1|");
    let team = eval.choice("team").unwrap();
    assert_eq!(team.choice, "billing");
    assert_eq!(team.confidence, 0.81, "the server's confidence wins");
    let mood = eval.score("mood").unwrap();
    assert_eq!(mood.score, 1.05);
    assert_eq!(mood.legend, vec!["Calm", "Frustrated", "Very angry"]);
    assert_eq!(mood.probabilities, vec![0.0, 0.95, 0.05]);
    assert_eq!(mood.level(), 1);
    assert_eq!(eval.usage.input_tokens, 1_000_000);
    assert_eq!(eval.usage.output_tokens, 34);
    // Priced by the versioned id the API reported: 1M input tokens at $0.042,
    // output free.
    let cost = eval.cost_usd.expect("jev-1.13.0 is in the price table");
    assert!((cost - 0.042).abs() < 1e-12, "{cost}");
    // Typed accessors refuse the wrong type.
    assert!(eval.choice("urgent").is_none());
    assert!(eval.noul("missing").is_none());

    // The wire body: the documented shape, options and questions in order.
    let received = server.received_requests().await.unwrap();
    let raw = String::from_utf8(received[0].body.clone()).unwrap();
    let body: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(
        body["state"],
        "Help! My payouts have been failing for 3 days."
    );
    assert_eq!(body["questions"]["urgent"]["type"], "noul");
    assert!(body["questions"]["urgent"].get("criteria").is_none());
    assert_eq!(
        body["questions"]["team"]["criteria"]["billing"],
        Value::Null
    );
    assert_eq!(
        body["questions"]["mood"]["criteria"],
        json!(["Calm", "Frustrated", "Very angry"])
    );
    let (b, t, s) = (
        raw.find("\"billing\"").unwrap(),
        raw.find("\"technical\"").unwrap(),
        raw.find("\"sales\"").unwrap(),
    );
    assert!(b < t && t < s, "choice options keep caller order: {raw}");
    let (u, tm, m) = (
        raw.find("\"urgent\"").unwrap(),
        raw.find("\"team\"").unwrap(),
        raw.find("\"mood\"").unwrap(),
    );
    assert!(u < tm && tm < m, "questions keep caller order: {raw}");
}

#[tokio::test]
async fn jevk5_style_answers_parse_leniently() {
    // A TypeSafe-style server with extras: noul confidence, per-answer
    // input_tokens, unknown fields, no top-level usage, no choice confidence,
    // no score/legend.
    let server = MockServer::start().await;
    mount_ok(
        &server,
        json!({
            "model": "jevk5-0.3",
            "latency_ms": 41,
            "answers": {
                "urgent": {"type": "noul", "noul": 0.8, "confidence": 0.7, "input_tokens": 120, "extra": true},
                "team": {"type": "choice", "probabilities": {"billing": 0.6, "technical": 0.3, "sales": 0.1}, "input_tokens": 130},
                "mood": {"type": "score", "probabilities": {"0": 0.1, "1": 0.2, "2": 0.7}, "input_tokens": 110}
            }
        }),
    )
    .await;

    let model = DecisionModel::local(server.uri());
    let eval = batched(&model).send().await.unwrap();

    assert_eq!(eval.model, "jevk5-0.3");
    assert_eq!(eval.noul("urgent").unwrap().confidence, 0.7);
    let team = eval.choice("team").unwrap();
    assert_eq!(team.choice, "billing", "argmax when choice is absent");
    // (3 * 0.6 - 1) / 2 = 0.4
    assert!((team.confidence - 0.4).abs() < 1e-9, "{}", team.confidence);
    let mood = eval.score("mood").unwrap();
    assert!((mood.score - 1.6).abs() < 1e-9, "sum(i * p_i)");
    // (3 * 0.7 - 1) / 2 = 0.55
    assert!((mood.confidence - 0.55).abs() < 1e-9);
    assert_eq!(mood.legend[2], "Very angry", "legend from the question");
    // Usage summed from per-answer counts when there is no top-level usage.
    assert_eq!(eval.usage.input_tokens, 360);
    // Local: $0, not unpriced.
    assert_eq!(eval.cost_usd, Some(0.0));
    // No key is sent to a local server.
    let received = server.received_requests().await.unwrap();
    assert!(received[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn one_line_conveniences() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(|req: &WireRequest| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let answer = match body["questions"]["q"]["type"].as_str().unwrap() {
                "noul" => json!({"type": "noul", "noul": 0.25}),
                "choice" => json!({"type": "choice", "choice": "b", "probabilities": {"a": 0.1, "b": 0.9}, "confidence": 0.8}),
                _ => json!({"type": "score", "score": 0.5, "legend": {"0": "lo", "1": "hi"}, "probabilities": {"0": 0.5, "1": 0.5}, "confidence": 0.0}),
            };
            ResponseTemplate::new(200).set_body_json(json!({"model": "jev-1.13.0", "answers": {"q": answer}, "usage": {"input_tokens": 10, "output_tokens": 1}}))
        })
        .expect(3)
        .mount(&server)
        .await;
    let jev = hosted(&server);
    assert_eq!(jev.noul("state", "yes?").await.unwrap(), 0.25);
    assert_eq!(
        jev.choice("state", "which?", ["a", "b"])
            .await
            .unwrap()
            .choice,
        "b"
    );
    assert_eq!(
        jev.score("state", "how?", ["lo", "hi"])
            .await
            .unwrap()
            .score,
        0.5
    );
}

#[tokio::test]
async fn structured_state_and_criteria_are_sent_as_json() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        json!({"model": "jev-1.13.0", "answers": {"dup": {"type": "noul", "noul": 0.1}}, "usage": {"input_tokens": 5, "output_tokens": 1}}),
    )
    .await;
    hosted(&server)
        .ask(json!({"resume": {"name": "John Smith"}}))
        .question(
            "dup",
            Question::noul_with_criteria(
                json!({"question": "Is `resume` the same person as `other`?", "other": {"name": "J. Smith"}}),
                "Same person",
                "Different people",
            ),
        )
        .send()
        .await
        .unwrap();
    let received = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["state"]["resume"]["name"], "John Smith");
    assert_eq!(
        body["questions"]["dup"]["instructions"]["other"]["name"],
        "J. Smith"
    );
    assert_eq!(body["questions"]["dup"]["criteria"]["true"], "Same person");
    assert_eq!(
        body["questions"]["dup"]["criteria"]["false"],
        "Different people"
    );
}

// ---------------------------------------------------------------------------
// Validation: rejected before anything is sent
// ---------------------------------------------------------------------------

async fn rejected(
    ask: impl std::future::Future<Output = Result<Evaluation, DecisionError>>,
) -> DecisionError {
    ask.await.expect_err("must be rejected client-side")
}

#[tokio::test]
async fn validation_limits_are_enforced_without_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let m = hosted(&server);

    // The positive controls (255 options, 10 levels) are in
    // `limits_accept_the_boundary`.
    let options: Vec<String> = (0..256).map(|i| format!("o{i}")).collect();
    let e = rejected(m.ask("s").choice("c", "which?", options.clone()).send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("256 options exceed")),
        "{e}"
    );
    let e = rejected(m.ask("s").choice("c", "which?", ["only"]).send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("at least 2 options")),
        "{e}"
    );
    let e = rejected(m.ask("s").choice("c", "which?", ["a", "a"]).send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("duplicate option")),
        "{e}"
    );
    let e = rejected(m.ask("s").score("sc", "how?", ["one"]).send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("2 to 10 levels")),
        "{e}"
    );
    let levels: Vec<String> = (0..11).map(|i| i.to_string()).collect();
    let e = rejected(m.ask("s").score("sc", "how?", levels).send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("got 11")),
        "{e}"
    );
    let e = rejected(m.ask("s").send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("at least one question")),
        "{e}"
    );
    let e = rejected(m.ask("s").noul("x", "a?").noul("x", "b?").send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("duplicate question id")),
        "{e}"
    );
    let e = rejected(m.ask("s").noul("x", "").send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("instructions")),
        "{e}"
    );
    let e = rejected(m.ask(json!(42)).noul("x", "a?").send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.starts_with("state")),
        "{e}"
    );
    // 32k tokens for the state plus the longest question (~4 bytes a token).
    let big = "word ".repeat(30_000);
    let e = rejected(m.ask(big).noul("x", "a?").send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("32000-token")),
        "{e}"
    );
    // 64k tokens for the whole request.
    let mid = "word ".repeat(16_000);
    let mut ask = m.ask("s");
    for i in 0..5 {
        ask = ask.noul(format!("q{i}"), mid.clone());
    }
    let e = rejected(ask.send()).await;
    assert!(
        matches!(&e, DecisionError::Invalid(t) if t.contains("64000-token")),
        "{e}"
    );
    // `server` verifies expect(0) on drop.
}

#[tokio::test]
async fn limits_accept_the_boundary() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|req: &WireRequest| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let q = &body["questions"]["c"];
            let answer = if q["type"] == "choice" {
                let n = q["criteria"].as_object().unwrap().len();
                let probs: serde_json::Map<String, Value> = q["criteria"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|k| (k.clone(), json!(1.0 / n as f64)))
                    .collect();
                json!({"type": "choice", "probabilities": probs})
            } else {
                let probs: serde_json::Map<String, Value> =
                    (0..10).map(|i| (i.to_string(), json!(0.1))).collect();
                json!({"type": "score", "probabilities": probs})
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"model": "m", "answers": {"c": answer}}))
        })
        .expect(2)
        .mount(&server)
        .await;
    let m = hosted(&server);
    let options: Vec<String> = (0..255).map(|i| format!("o{i}")).collect();
    let eval = m
        .ask("s")
        .choice("c", "which?", options)
        .send()
        .await
        .unwrap();
    let a = eval.choice("c").unwrap();
    assert!(
        a.confidence.abs() < 1e-9,
        "uniform over 255 is zero confidence"
    );
    let levels: Vec<String> = (0..10).map(|i| i.to_string()).collect();
    let eval = m.ask("s").score("c", "how?", levels).send().await.unwrap();
    assert!((eval.score("c").unwrap().score - 4.5).abs() < 1e-9);
}

#[tokio::test]
async fn unsupported_question_type_is_an_error_not_an_emulation() {
    let mock = MockBackend::neutral().with_capabilities(Capabilities::new([QuestionKind::Noul]));
    let model = DecisionModel::from_backend(mock.clone(), "m");
    let e = model
        .ask("s")
        .noul("ok", "fine?")
        .score("s", "how?", ["lo", "hi"])
        .send()
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DecisionError::Unsupported(t) if t.contains("questions.s")),
        "{e}"
    );
    assert_eq!(mock.request_count(), 0, "nothing sent");
    // Positive control: the supported question alone goes through.
    model.ask("s").noul("ok", "fine?").send().await.unwrap();
    assert_eq!(mock.request_count(), 1);
}

// ---------------------------------------------------------------------------
// Retries and error mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retries_429_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    mount_ok(&server, typesafe_response()).await;
    let eval = batched(&hosted(&server)).send().await.unwrap();
    assert_eq!(eval.model, "jev-1.13.0");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn retries_529_overloaded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(529))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    mount_ok(&server, typesafe_response()).await;
    batched(&hosted(&server)).send().await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn retry_after_wins_over_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0.05"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_ok(&server, typesafe_response()).await;
    // A 20 s backoff: finishing fast proves the 50 ms retry-after was used.
    let model = DecisionModel::from_systemone(
        SystemOneBackend::typesafe()
            .with_base_url(server.uri())
            .with_api_key(KEY)
            .with_retry(RetryConfig {
                max_retries: 1,
                initial_delay_ms: 20_000,
                backoff_multiplier: 1.0,
                max_delay_ms: 30_000,
            }),
        "jev-latest",
    );
    let start = Instant::now();
    batched(&model).send().await.unwrap();
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(40), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[tokio::test]
async fn exhausted_retries_report_rate_limited_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0.01"))
        .expect(4)
        .mount(&server)
        .await;
    let e = batched(&hosted(&server)).send().await.unwrap_err();
    match e {
        DecisionError::RateLimited {
            status,
            retry_after,
            ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(retry_after, Some(Duration::from_millis(10)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn error_statuses_map_to_typed_errors_without_retrying() {
    for (status, body, check) in [
        (
            401u16,
            r#"{"error":"invalid api key"}"#,
            (|e: &DecisionError| matches!(e, DecisionError::Http { status: 401, body, .. } if body.contains("invalid api key")))
                as fn(&DecisionError) -> bool,
        ),
        (
            422,
            r#"{"detail":"questions.team.criteria: too many options"}"#,
            |e| matches!(e, DecisionError::Invalid(t) if t.contains("questions.team.criteria")),
        ),
        (500, "boom", |e| {
            matches!(e, DecisionError::Http { status: 500, .. })
        }),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        let e = batched(&hosted(&server)).send().await.unwrap_err();
        assert!(check(&e), "HTTP {status} mapped to {e:?}");
        assert!(
            !e.to_string().contains(KEY),
            "the key never appears in errors"
        );
    }
}

#[tokio::test]
async fn unusable_responses_are_bad_response() {
    for body in [
        json!({"model": "m", "answers": {"urgent": {"type": "noul", "noul": 0.9}}}),
        json!({"model": "m"}),
        json!({"model": "m", "answers": {
            "urgent": {"type": "choice", "noul": 0.9},
            "team": {"type": "choice", "probabilities": {"billing": 1.0}},
            "mood": {"type": "score", "probabilities": {"0": 1.0}}}}),
        json!({"model": "m", "answers": {
            "urgent": {"type": "noul", "noul": 1.7},
            "team": {"type": "choice", "probabilities": {"billing": 1.0}},
            "mood": {"type": "score", "probabilities": {"0": 1.0}}}}),
        json!({"model": "m", "answers": {
            "urgent": {"type": "noul", "noul": 0.2},
            "team": {"type": "choice", "probabilities": {"refunds": 1.0}},
            "mood": {"type": "score", "probabilities": {"0": 1.0}}}}),
        json!({"model": "m", "answers": {
            "urgent": {"type": "noul", "noul": 0.2},
            "team": {"type": "choice", "probabilities": {"billing": 1.0}},
            "mood": {"type": "score", "probabilities": {"7": 1.0}}}}),
    ] {
        let server = MockServer::start().await;
        mount_ok(&server, body.clone()).await;
        let e = batched(&hosted(&server)).send().await.unwrap_err();
        assert!(
            matches!(e, DecisionError::BadResponse(_)),
            "{body} -> {e:?}"
        );
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    let e = batched(&hosted(&server)).send().await.unwrap_err();
    assert!(
        matches!(&e, DecisionError::BadResponse(t) if t.contains("not JSON")),
        "{e:?}"
    );
}

#[tokio::test]
async fn missing_env_key_fails_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let model = DecisionModel::from_systemone(
        SystemOneBackend::typesafe()
            .with_base_url(server.uri())
            .with_api_key_env("YOAGENT_DECISION_TEST_NEVER_SET"),
        "jev-latest",
    );
    let e = model.noul("s", "q?").await.unwrap_err();
    assert_eq!(
        e,
        DecisionError::MissingApiKey("YOAGENT_DECISION_TEST_NEVER_SET".into())
    );
}

#[tokio::test]
async fn timeout_bounds_the_whole_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(typesafe_response())
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let model = hosted(&server).with_timeout(Duration::from_millis(100));
    let start = Instant::now();
    let e = batched(&model).send().await.unwrap_err();
    assert_eq!(e, DecisionError::Timeout(Duration::from_millis(100)));
    assert!(start.elapsed() < Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// Backends, presets, pricing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_backend_records_requests_and_scripts_answers() {
    let mock = MockBackend::new().push(
        Evaluation::new("mock-1", DecisionUsage::new(7, 0))
            .with_answer("a", NoulAnswer::new(0.9))
            .with_answer("b", ChoiceAnswer::new([("x", 0.2), ("y", 0.8)])),
    );
    let model = DecisionModel::from_backend(mock.clone(), "mock");
    let eval = model
        .ask("state")
        .noul("a", "yes?")
        .choice("b", "which?", ["x", "y"])
        .send()
        .await
        .unwrap();
    assert_eq!(eval.p_true("a"), Some(0.9));
    assert_eq!(eval.choice("b").unwrap().choice, "y");
    assert_eq!(eval.cost_usd, Some(0.0), "the mock is a local backend");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].model, "mock");
    assert_eq!(reqs[0].questions.len(), 2);
    // Exhausted with no responder: an error, not a silent default.
    assert!(matches!(
        model.noul("s", "q?").await,
        Err(DecisionError::BadResponse(_))
    ));
    // A scripted answer of the wrong type is caught by the model handle.
    let wrong = MockBackend::new().push(
        Evaluation::new("m", DecisionUsage::default()).with_answer("q", NoulAnswer::new(0.5)),
    );
    let e = DecisionModel::from_backend(wrong, "m")
        .choice("s", "which?", ["a", "b"])
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DecisionError::BadResponse(t) if t.contains("a noul answer to a choice question")),
        "{e}"
    );
}

#[tokio::test]
async fn a_non_batching_backend_gets_one_request_per_question() {
    let mock = MockBackend::from_fn(|req| {
        let (id, _) = &req.questions[0];
        Ok(Evaluation::new("m", DecisionUsage::new(10, 1))
            .with_answer(id.clone(), NoulAnswer::new(0.3)))
    })
    .with_capabilities(Capabilities::new(QuestionKind::all()).with_batching(false));
    let eval = DecisionModel::from_backend(mock.clone(), "m")
        .ask("s")
        .noul("a", "a?")
        .noul("b", "b?")
        .noul("c", "c?")
        .send()
        .await
        .unwrap();
    assert_eq!(mock.request_count(), 3);
    assert!(mock.requests().iter().all(|r| r.questions.len() == 1));
    assert_eq!(eval.answers.len(), 3);
    assert_eq!(eval.usage, DecisionUsage::new(30, 3));
}

#[tokio::test]
async fn pricing_follows_the_reported_model() {
    let server = MockServer::start().await;
    let mut body = typesafe_response();
    body["model"] = json!("jev-9.9.9");
    mount_ok(&server, body).await;
    // An unlisted version is unpriced, never guessed.
    let eval = batched(&hosted(&server)).send().await.unwrap();
    assert_eq!(eval.cost_usd, None);

    let m = DecisionModel::jev();
    let usage = DecisionUsage::new(2_000_000, 500);
    assert!((m.cost_usd("jev-1.13.0", &usage).unwrap() - 0.084).abs() < 1e-12);
    assert_eq!(
        m.cost_usd("jev-latest", &usage),
        None,
        "aliases are unpriced"
    );
    assert_eq!(
        DecisionModel::jev_opencode().cost_usd("jev-1.13.0", &usage),
        None
    );
    assert_eq!(
        DecisionModel::local("http://localhost:1").cost_usd("x", &usage),
        Some(0.0)
    );
    let fixed = DecisionModel::jev_opencode_free()
        .with_cost(Some(yoagent::provider::CostConfig::new(0.0, 0.0)));
    assert_eq!(fixed.cost_usd("jev-1.13-free", &usage), Some(0.0));
}

#[test]
fn presets_are_configured_but_inert() {
    let jev = DecisionModel::jev();
    assert_eq!(jev.model(), "jev-latest");
    assert!(jev.capabilities().native_confidence);
    assert_eq!(jev.capabilities().max_choice_options, 255);
    assert_eq!(DecisionModel::jev_opencode().model(), "jev-1.13");
    assert_eq!(DecisionModel::jev_opencode_free().model(), "jev-1.13-free");
    assert!(DecisionModel::local("http://x").capabilities().local);
    assert_eq!(jev.clone().with_model("jev-1.13.0").model(), "jev-1.13.0");
    assert_eq!(
        SystemOneBackend::opencode_zen().endpoint_url(),
        "https://opencode.ai/zen/v1/systemone"
    );
    for base in [
        "http://localhost:8000",
        "http://localhost:8000/",
        "http://localhost:8000/v1",
        "http://localhost:8000/v1/systemone",
    ] {
        assert_eq!(
            SystemOneBackend::new(base).endpoint_url(),
            "http://localhost:8000/v1/systemone"
        );
    }
    // Debug output never carries a key.
    let dbg = format!("{:?}", jev.with_api_key("sk-very-secret"));
    assert!(!dbg.contains("sk-very-secret"), "{dbg}");
    assert!(dbg.contains("redacted"), "{dbg}");
}

#[tokio::test]
async fn list_models_reads_the_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"name": "jev-latest", "description": "Flagship", "release_date": "2026-09-01"},
                {"name": "jev-preview"}
            ]
        })))
        .mount(&server)
        .await;
    let backend = SystemOneBackend::typesafe()
        .with_base_url(server.uri())
        .with_api_key(KEY);
    let models = backend.list_models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].name, "jev-latest");
    assert_eq!(models[0].description.as_deref(), Some("Flagship"));
    assert_eq!(models[1].release_date, None);
}
