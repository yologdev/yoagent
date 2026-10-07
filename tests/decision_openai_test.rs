//! OpenAI's Decisions API, offline: the exact request yoagent sends to
//! `/v1/decisions`, each answer kind read back, refusals, usage, errors and
//! the key variable. Mock-only — no live OpenAI key was available.
//!
//! Its own binary because some tests set `OPENAI_API_KEY`; they hold `ENV`
//! so they never interleave.

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::decision::*;
use yoagent::retry::RetryConfig;
use yoagent::{ToolCallRequest, ToolDecision};

static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn backend(server: &MockServer) -> OpenAiDecisionBackend {
    OpenAiDecisionBackend::new()
        .with_base_url(format!("{}/v1", server.uri()))
        .with_retry(RetryConfig::none())
}

fn model(server: &MockServer) -> DecisionModel {
    DecisionModel::from_openai_backend(backend(server).with_api_key("sk-test"), "gpt-6-luna")
}

fn usage() -> Value {
    json!({
        "input_tokens": 412,
        "input_tokens_details": {"cache_write_tokens": 0, "cached_tokens": 128},
        "output_tokens": 9,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": 421
    })
}

fn three_answers() -> Value {
    json!({
        "model": "gpt-6-luna",
        "answers": [
            {"type": "predicate", "name": "urgent", "probability": 0.93},
            {
                "type": "choice", "name": "team", "choice": "technical",
                "probabilities": [
                    {"value": "billing", "probability": 0.04},
                    {"value": "technical", "probability": 0.9},
                    {"value": "sales", "probability": 0.06}
                ],
                "confidence": 0.85
            },
            {
                "type": "score", "name": "severity", "score": 1.1,
                "probabilities": [
                    {"value": 0, "label": "Minor", "probability": 0.1},
                    {"value": 1, "label": "Major", "probability": 0.7},
                    {"value": 2, "label": "Critical", "probability": 0.2}
                ],
                "confidence": 0.55
            }
        ],
        "usage": usage()
    })
}

async fn serve(body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

async fn ask_three(model: &DecisionModel) -> Result<Evaluation, DecisionError> {
    model
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
            ["Minor", "Major", "Critical"],
        )
        .send()
        .await
}

#[tokio::test]
async fn sends_the_documented_request_and_reads_every_answer_kind() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .and(header("authorization", "Bearer sk-test"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(three_answers()))
        .expect(1)
        .mount(&server)
        .await;

    let eval = ask_three(&model(&server)).await.unwrap();

    let sent: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(
        sent,
        json!({
            "model": "gpt-6-luna",
            "input": "Checkout has been failing for every customer for the last hour.",
            "questions": [
                {"type": "predicate", "name": "urgent",
                 "instructions": "Is this support request urgent?"},
                {"type": "choice", "name": "team",
                 "instructions": "Which team should handle this request?",
                 "choices": [
                    {"value": "billing", "description": "Payments, invoices, and refunds"},
                    {"value": "technical", "description": "Outages, errors, and configuration"},
                    {"value": "sales", "description": "Plans and upgrades"}
                 ]},
                {"type": "score", "name": "severity",
                 "instructions": "How severe is the customer impact?",
                 "levels": [{"label": "Minor"}, {"label": "Major"}, {"label": "Critical"}]}
            ]
        })
    );

    assert_eq!(eval.model(), "gpt-6-luna");
    let urgent = eval.noul("urgent").unwrap();
    assert!((urgent.p_true() - 0.93).abs() < 1e-12);
    // No confidence is returned for a predicate: |2p - 1|.
    assert!((urgent.confidence() - 0.86).abs() < 1e-9);

    let team = eval.choice("team").unwrap();
    assert_eq!(team.choice(), "technical");
    assert!((team.confidence() - 0.85).abs() < 1e-12);
    assert_eq!(
        team.probabilities().collect::<Vec<_>>(),
        [("billing", 0.04), ("technical", 0.9), ("sales", 0.06)]
    );

    let severity = eval.score("severity").unwrap();
    assert!((severity.score() - 1.1).abs() < 1e-12);
    assert_eq!(severity.probabilities(), [0.1, 0.7, 0.2]);
    assert_eq!(severity.legend(), ["Minor", "Major", "Critical"]);
    assert_eq!(severity.level(), 1);
    assert!((severity.confidence() - 0.55).abs() < 1e-12);

    // Usage: input and output tokens; unpriced off api.openai.com.
    assert_eq!(eval.usage(), DecisionUsage::new(412, 9));
    assert_eq!(eval.cost_usd(), None);
}

#[tokio::test]
async fn a_json_state_is_sent_as_readable_text_and_options_keep_their_order() {
    let server = serve(json!({
        "answers": [{
            "type": "choice", "name": "q", "choice": "b",
            "probabilities": [{"value": "b", "probability": 0.7}, {"value": "a", "probability": 0.3}],
            "confidence": 0.4
        }],
        "model": "gpt-6-luna",
        "usage": usage()
    }))
    .await;
    let answer = model(&server)
        .choice(
            json!({"ticket": 42, "text": "refund"}),
            "Which?",
            ["a", "b"],
        )
        .await
        .unwrap();
    // Option order, whatever order the server listed them in.
    assert_eq!(
        answer.probabilities().collect::<Vec<_>>(),
        [("a", 0.3), ("b", 0.7)]
    );
    let sent: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(
        sent["input"],
        "{\n  \"text\": \"refund\",\n  \"ticket\": 42\n}"
    );
    assert_eq!(sent["questions"][0]["name"], "q");
    assert_eq!(
        sent["questions"][0]["choices"],
        json!([{"value": "a"}, {"value": "b"}])
    );
}

#[tokio::test]
async fn unnamed_answers_are_matched_by_position_only_when_counts_match() {
    let unnamed = json!({
        "model": "gpt-6-luna",
        "answers": [
            {"type": "predicate", "probability": 0.2},
            {"type": "predicate", "name": null, "probability": 0.8}
        ],
        "usage": usage()
    });
    let server = serve(unnamed).await;
    let eval = model(&server)
        .ask("s")
        .noul("first", "One?")
        .noul("second", "Two?")
        .send()
        .await
        .unwrap();
    assert_eq!(eval.p_true("first"), Some(0.2));
    assert_eq!(eval.p_true("second"), Some(0.8));

    // One answer for two questions: no positional guess.
    let short = serve(json!({
        "answers": [{"type": "predicate", "probability": 0.2}],
        "usage": usage()
    }))
    .await;
    let err = model(&short)
        .ask("s")
        .noul("first", "One?")
        .noul("second", "Two?")
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, DecisionError::BadResponse(_)), "{err:?}");
}

#[tokio::test]
async fn a_refusal_is_refused_naming_the_question_and_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "gpt-6-luna",
            "answers": [
                {"type": "refusal", "name": "urgent"}
            ],
            "usage": usage()
        })))
        .expect(1)
        .mount(&server)
        .await;
    let retrying = RetryConfig {
        max_retries: 3,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 1,
    };
    let model = DecisionModel::from_openai_backend(
        backend(&server).with_api_key("k").with_retry(retrying),
        "gpt-6-luna",
    );
    let err = model
        .ask("s")
        .noul("urgent", "Is it urgent?")
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, DecisionError::Refused { .. }), "{err:?}");
    assert!(!err.is_retryable());
    assert!(err.to_string().contains("`urgent`"), "{err}");
    assert!(err.to_string().contains("refusal"), "{err}");
}

#[tokio::test]
async fn the_tool_gate_denies_when_openai_refuses() {
    let server = serve(json!({
        "model": "gpt-6-luna",
        "answers": [
            {"type": "refusal", "name": "destructive"},
            {"type": "predicate", "name": "requested", "probability": 0.99}
        ],
        "usage": usage()
    }))
    .await;
    let gate = ToolGate::new(model(&server));
    let args = json!({"path": "/srv/data"});
    let prompts = [yoagent::Message::user("clean up the data dir")];
    let call = ToolCallRequest::new("call-1", "rm", &args).with_run_prompts(&prompts);
    match gate.decide(&call).await {
        ToolDecision::Deny(reason) => assert!(reason.contains("refusal"), "{reason}"),
        other => panic!("expected a denial, got {other:?}"),
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn unusable_answers_are_bad_responses() {
    let cases = [
        // Missing answer.
        json!({"answers": [], "usage": usage()}),
        // Wrong kind.
        json!({"answers": [{"type": "score", "name": "q", "score": 0.5,
               "probabilities": [], "confidence": 0.5}], "usage": usage()}),
        // Probability out of range (caught by the central check).
        json!({"answers": [{"type": "predicate", "name": "q", "probability": 1.7}],
               "usage": usage()}),
        // Not a number.
        json!({"answers": [{"type": "predicate", "name": "q", "probability": "high"}],
               "usage": usage()}),
        // Missing probability.
        json!({"answers": [{"type": "predicate", "name": "q"}], "usage": usage()}),
        // No answers array.
        json!({"answers": {"q": {"type": "predicate", "probability": 0.5}}}),
    ];
    for body in cases {
        let server = serve(body.clone()).await;
        let err = model(&server).noul("s", "Is it?").await.unwrap_err();
        assert!(
            matches!(err, DecisionError::BadResponse(_)),
            "{body}: {err:?}"
        );
    }

    let choice_cases = [
        // A boolean value is not one of our (string) options.
        json!({"answers": [{"type": "choice", "name": "q", "choice": true, "confidence": 1.0,
               "probabilities": [{"value": true, "probability": 1.0}]}], "usage": usage()}),
        // An option missing from the distribution.
        json!({"answers": [{"type": "choice", "name": "q", "choice": "a", "confidence": 1.0,
               "probabilities": [{"value": "a", "probability": 1.0}]}], "usage": usage()}),
        // A distribution that does not sum to 1.
        json!({"answers": [{"type": "choice", "name": "q", "choice": "a", "confidence": 1.0,
               "probabilities": [{"value": "a", "probability": 0.9},
                                 {"value": "b", "probability": 0.9}]}], "usage": usage()}),
    ];
    for body in choice_cases {
        let server = serve(body.clone()).await;
        let err = model(&server)
            .choice("s", "Which?", ["a", "b"])
            .await
            .unwrap_err();
        assert!(
            matches!(err, DecisionError::BadResponse(_)),
            "{body}: {err:?}"
        );
    }

    let score_cases = [
        // A level index past the last level.
        json!({"answers": [{"type": "score", "name": "q", "score": 1.0, "confidence": 1.0,
               "probabilities": [{"value": 0, "label": "lo", "probability": 0.0},
                                 {"value": 2, "label": "x", "probability": 1.0}]}],
               "usage": usage()}),
        // A level left out.
        json!({"answers": [{"type": "score", "name": "q", "score": 0.0, "confidence": 1.0,
               "probabilities": [{"value": 0, "label": "lo", "probability": 1.0}]}],
               "usage": usage()}),
    ];
    for body in score_cases {
        let server = serve(body.clone()).await;
        let err = model(&server)
            .score("s", "How much?", ["lo", "hi"])
            .await
            .unwrap_err();
        assert!(
            matches!(err, DecisionError::BadResponse(_)),
            "{body}: {err:?}"
        );
    }
}

#[tokio::test]
async fn a_response_without_usage_is_unpriced_even_with_a_cost() {
    let server = serve(json!({
        "answers": [{"type": "predicate", "name": "q", "probability": 0.5}]
    }))
    .await;
    let priced = model(&server).with_cost(Some(yoagent::provider::CostConfig::new(0.1, 0.0)));
    let eval = priced.ask("s").noul("q", "Is it?").send().await.unwrap();
    assert_eq!(eval.cost_usd(), None);
    assert_eq!(eval.usage(), DecisionUsage::new(0, 0));
}

#[tokio::test]
async fn rate_limits_are_retried_then_reported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "Rate limit reached for gpt-6-luna", "type": "requests"}
        })))
        .expect(3)
        .mount(&server)
        .await;
    let retrying = RetryConfig {
        max_retries: 2,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 1,
    };
    let model =
        DecisionModel::from_openai_backend(backend(&server).with_api_key("k"), "gpt-6-luna")
            .with_retry(retrying);
    let err = model.noul("s", "Is it?").await.unwrap_err();
    assert!(
        matches!(err, DecisionError::RateLimited { status: 429, .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("Rate limit reached"), "{err}");
}

#[tokio::test]
async fn a_rate_limit_then_success_answers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "answers": [{"type": "predicate", "name": "q", "probability": 0.25}],
            "usage": usage()
        })))
        .mount(&server)
        .await;
    let model = model(&server).with_retry(RetryConfig {
        max_retries: 2,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 1,
    });
    let answer = model.noul("s", "Is it?").await.unwrap();
    assert_eq!(answer.p_true(), 0.25);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_rejected_key_names_openai_api_key_and_never_the_value() {
    let _env = ENV.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": "Incorrect API key provided", "code": "invalid_api_key"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    std::env::set_var("OPENAI_API_KEY", " sk-wrong\n");
    let model = DecisionModel::from_openai_backend(backend(&server), "gpt-6-luna");
    let result = model.noul("s", "q").await;
    std::env::remove_var("OPENAI_API_KEY");
    let err = result.unwrap_err();
    assert!(
        matches!(err, DecisionError::Http { status: 401, .. }),
        "{err:?}"
    );
    assert!(!err.is_retryable());
    assert!(err.to_string().contains("$OPENAI_API_KEY"), "{err}");
    assert!(!err.to_string().contains("sk-wrong"), "{err}");
    // The key was trimmed before it was sent.
    let sent = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        sent.headers.get("authorization").unwrap(),
        "Bearer sk-wrong"
    );
}

#[tokio::test]
async fn no_key_fails_before_sending() {
    let _env = ENV.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let model = DecisionModel::from_openai_backend(backend(&server), "gpt-6-luna");
    std::env::remove_var("OPENAI_API_KEY");
    match model.noul("s", "q").await {
        Err(DecisionError::MissingApiKey(var)) => assert_eq!(var, "OPENAI_API_KEY"),
        other => panic!("expected MissingApiKey, got {other:?}"),
    }
    std::env::set_var("OPENAI_API_KEY", "  ");
    let result = model.noul("s", "q").await;
    std::env::remove_var("OPENAI_API_KEY");
    match result {
        Err(DecisionError::MissingApiKey(var)) => {
            assert_eq!(var, "OPENAI_API_KEY (set, but empty)")
        }
        other => panic!("expected MissingApiKey, got {other:?}"),
    }
}

#[tokio::test]
async fn a_safety_identifier_is_sent_when_set() {
    let server = serve(json!({
        "answers": [{"type": "predicate", "name": "q", "probability": 0.5}],
        "usage": usage()
    }))
    .await;
    let model = DecisionModel::from_openai_backend(
        backend(&server)
            .with_api_key("k")
            .with_safety_identifier("user-7f3a"),
        "gpt-6-luna",
    );
    model.noul("s", "q").await.unwrap();
    let sent: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(sent["safety_identifier"], "user-7f3a");
}

#[test]
fn the_preset_asks_for_gpt_6_luna_on_openai() {
    let luna = DecisionModel::gpt_6_luna();
    assert_eq!(luna.model(), "gpt-6-luna");
    assert!(luna.capabilities().batching);
    assert!(!luna.capabilities().local);
    let debug = format!("{luna:?}");
    assert!(
        debug.contains("https://api.openai.com/v1/decisions"),
        "{debug}"
    );
    assert!(debug.contains("$OPENAI_API_KEY"), "{debug}");
    // Another model id, same endpoint.
    assert_eq!(
        luna.with_model("gpt-6-luna-next").model(),
        "gpt-6-luna-next"
    );
}

/// Off `api.openai.com` the bundled price never applies, even once
/// enabled: a proxy's bill is not OpenAI's list price.
#[tokio::test]
async fn bundled_pricing_applies_only_on_openais_host() {
    yoagent::provider::prices::enable_bundled();
    let server = serve(three_answers()).await;
    let eval = ask_three(&model(&server)).await.unwrap();
    assert_eq!(eval.cost_usd(), None);
}

// ---- Spend of failed evaluations, refusals in chains, empty keys ----

/// A tool the gate is asked about; counts its runs.
struct Rm(std::sync::Arc<std::sync::Mutex<usize>>);

#[async_trait::async_trait]
impl yoagent::AgentTool for Rm {
    fn name(&self) -> &str {
        "rm"
    }
    fn label(&self) -> &str {
        "rm"
    }
    fn description(&self) -> &str {
        "Remove a file."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: Value,
        _ctx: yoagent::ToolContext,
    ) -> Result<yoagent::ToolResult, yoagent::ToolError> {
        *self.0.lock().unwrap() += 1;
        Ok(yoagent::ToolResult {
            content: vec![yoagent::Content::Text { text: "ok".into() }],
            details: Value::Null,
        })
    }
}

/// One agent run whose single `rm` call is judged by a `ToolGate` over
/// `model`: whether `rm` ran, and the run's stats.
async fn gated_run(model: DecisionModel) -> (usize, yoagent::SessionStats) {
    use yoagent::provider::mock::{MockResponse, MockToolCall};
    use yoagent::provider::{MockProvider, ModelConfig};
    let ran = std::sync::Arc::new(std::sync::Mutex::new(0));
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "rm".into(),
            arguments: json!({"path": "/tmp/x"}),
        }]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = yoagent::Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Rm(ran.clone()))])
        .with_tool_gate(ToolGate::new(model));
    let mut rx = agent.prompt("summarize the README").await;
    let mut stats = None;
    while let Some(e) = rx.recv().await {
        if let yoagent::AgentEvent::AgentEnd { stats: s, .. } = e {
            stats = Some(s);
        }
    }
    agent.finish().await;
    let n = *ran.lock().unwrap();
    (n, stats.expect("AgentEnd"))
}

/// $0.10 per million input tokens, output free (the gpt-6-luna list price).
fn luna_rate() -> Option<yoagent::provider::CostConfig> {
    Some(yoagent::provider::CostConfig::new(0.1, 0.0))
}

fn refusing() -> Value {
    json!({
        "model": "gpt-6-luna",
        "answers": [
            {"type": "refusal", "name": "destructive"},
            {"type": "predicate", "name": "requested", "probability": 0.99}
        ],
        "usage": usage()
    })
}

#[tokio::test]
async fn a_refused_evaluation_records_its_usage_in_the_run() {
    // Priced: the refusal's 412 input tokens are spent and priced.
    let server = serve(refusing()).await;
    let (ran, stats) = gated_run(model(&server).with_cost(luna_rate())).await;
    assert_eq!(ran, 0, "the gate fails closed on a refusal");
    let d = &stats.decision;
    assert_eq!((d.requests, d.failures, d.timeouts), (1, 1, 0));
    assert_eq!((d.usage.input, d.usage.output), (412, 9));
    assert_eq!(d.unpriced, 0);
    let cost = d.cost_usd.expect("priced");
    assert!((cost - 412.0 * 0.1 / 1e6).abs() < 1e-15, "{cost}");
    assert!(!d.is_unpriced());
    assert_eq!(stats.total_cost_usd(), Some(cost));

    // Unpriced (off api.openai.com, no cost set): the usage is still there,
    // and the spend reads as unknown, never $0.
    let server = serve(refusing()).await;
    let (_, stats) = gated_run(model(&server)).await;
    let d = &stats.decision;
    assert_eq!((d.usage.input, d.usage.output), (412, 9));
    assert_eq!(d.unpriced, 1);
    assert_eq!(d.cost_usd, None);
    assert!(d.is_unpriced());
}

#[tokio::test]
async fn an_unusable_answer_still_records_its_usage() {
    let bodies = [
        // Rejected while parsing: not a number.
        json!({"model": "gpt-6-luna", "answers": [
            {"type": "predicate", "name": "destructive", "probability": "high"},
            {"type": "predicate", "name": "requested", "probability": 0.9}
        ], "usage": usage()}),
        // Rejected by the central check: out of range.
        json!({"model": "gpt-6-luna", "answers": [
            {"type": "predicate", "name": "destructive", "probability": 1.7},
            {"type": "predicate", "name": "requested", "probability": 0.9}
        ], "usage": usage()}),
    ];
    for body in bodies {
        let server = serve(body.clone()).await;
        let (ran, stats) = gated_run(model(&server).with_cost(luna_rate())).await;
        assert_eq!(ran, 0, "{body}");
        let d = &stats.decision;
        assert_eq!((d.requests, d.failures), (1, 1), "{body}");
        assert_eq!(d.usage.input, 412, "{body}");
        assert_eq!(d.unpriced, 0, "{body}");
        assert!(d.cost_usd.is_some_and(|c| c > 0.0), "{body}: {d:?}");
    }
}

#[tokio::test]
async fn a_success_whose_spend_is_unknown_counts_as_unpriced_not_free() {
    // A 2xx whose body is not JSON was answered (and billed) — at an
    // unknown price, even under a cost.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>"))
        .mount(&server)
        .await;
    let (ran, stats) = gated_run(model(&server).with_cost(luna_rate())).await;
    assert_eq!(ran, 0);
    let d = &stats.decision;
    assert_eq!((d.requests, d.failures, d.unpriced), (1, 1, 1));
    assert_eq!(d.cost_usd, None);
    assert!(d.is_unpriced());

    // A failure with no answer (a 500) spent nothing: nothing unpriced.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let (_, stats) = gated_run(model(&server).with_cost(luna_rate())).await;
    let d = &stats.decision;
    assert_eq!((d.requests, d.failures, d.unpriced), (1, 1, 0));
    assert!(!d.is_unpriced());
}

#[tokio::test]
async fn a_refusal_ends_a_fallback_chain() {
    let server = serve(refusing()).await;
    let fallback = MockBackend::neutral();
    let chain = model(&server).or(DecisionModel::from_backend(fallback.clone(), "fallback"));
    let err = chain
        .ask("s")
        .noul("destructive", "Destructive?")
        .noul("requested", "Requested?")
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, DecisionError::Refused { .. }), "{err:?}");
    assert!(err.is_refusal());
    assert!(!err.is_retryable());
    assert!(err.to_string().contains("`destructive`"), "{err}");
    assert_eq!(
        fallback.request_count(),
        0,
        "never asked what OpenAI declined"
    );

    // Through the gate: denied, one request recorded with its usage.
    let fallback = MockBackend::neutral();
    let server = serve(refusing()).await;
    let chain = model(&server)
        .with_cost(luna_rate())
        .or(DecisionModel::from_backend(fallback.clone(), "fallback"));
    let (ran, stats) = gated_run(chain).await;
    assert_eq!(ran, 0);
    assert_eq!(fallback.request_count(), 0);
    assert_eq!((stats.decision.requests, stats.decision.failures), (1, 1));
    assert_eq!(stats.decision.usage.input, 412);
}

#[tokio::test]
async fn an_empty_fixed_key_fails_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(three_answers()))
        .expect(0)
        .mount(&server)
        .await;
    for key in ["", "   ", "\n\t"] {
        let model =
            DecisionModel::from_openai_backend(backend(&server).with_api_key(key), "gpt-6-luna");
        match model.noul("s", "q").await {
            Err(DecisionError::MissingApiKey(what)) => {
                assert!(what.contains("with_api_key"), "{what}")
            }
            other => panic!("{key:?}: expected MissingApiKey, got {other:?}"),
        }
        // The same through the handle's own setter.
        let model =
            DecisionModel::from_openai_backend(backend(&server), "gpt-6-luna").with_api_key(key);
        assert!(
            matches!(
                model.noul("s", "q").await,
                Err(DecisionError::MissingApiKey(_))
            ),
            "{key:?}"
        );
    }
}

#[tokio::test]
async fn two_entries_for_one_option_are_a_bad_response() {
    let server = serve(json!({
        "answers": [{"type": "choice", "name": "q", "choice": "a", "confidence": 0.5,
            "probabilities": [
                {"value": "a", "probability": 0.5},
                {"value": "a", "probability": 0.2},
                {"value": "b", "probability": 0.3}
            ]}],
        "usage": usage()
    }))
    .await;
    let err = model(&server)
        .choice("s", "Which?", ["a", "b"])
        .await
        .unwrap_err();
    match err {
        DecisionError::BadResponse(text) => {
            assert!(text.contains("two entries for \"a\""), "{text}")
        }
        other => panic!("expected BadResponse, got {other:?}"),
    }
}
