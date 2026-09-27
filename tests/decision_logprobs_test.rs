//! The logprob decision backend against a wiremock OpenAI-compatible server:
//! the request shape, label parsing for every question type, renormalising,
//! temperature scaling, the Choice limit, fan-out, usage, pricing, retries.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WireRequest, ResponseTemplate};
use yoagent::decision::*;
use yoagent::retry::RetryConfig;

const CHAT: &str = "/v1/chat/completions";

fn fast_retry() -> RetryConfig {
    RetryConfig {
        max_retries: 3,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 50,
    }
}

/// A chat completion whose first token's `top_logprobs` are `top`
/// (probabilities, logged here).
fn completion(top: &[(&str, f64)], usage: (u64, u64)) -> Value {
    let list: Vec<Value> = top
        .iter()
        .map(|(t, p)| json!({"token": t, "logprob": p.ln(), "bytes": t.as_bytes()}))
        .collect();
    let first = top.first().map_or("?", |(t, _)| *t);
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "qwen3-8b-q4",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": first},
            "logprobs": {"content": [{"token": first, "logprob": -0.1, "top_logprobs": list}]},
            "finish_reason": "length"
        }],
        "usage": {"prompt_tokens": usage.0, "completion_tokens": usage.1, "total_tokens": usage.0 + usage.1}
    })
}

async fn mount(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path(CHAT))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn body_of(req: &WireRequest) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn prompt_of(req: &WireRequest) -> String {
    body_of(req)["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

fn model(server: &MockServer) -> DecisionModel {
    DecisionModel::logprobs(server.uri(), "llama-3.1-8b-instruct").with_retry(fast_retry())
}

/// A model over a backend configured by `f`.
fn model_with(
    server: &MockServer,
    f: impl FnOnce(LogprobBackend) -> LogprobBackend,
) -> DecisionModel {
    DecisionModel::from_logprob_backend(
        f(LogprobBackend::new(server.uri())),
        "llama-3.1-8b-instruct",
    )
    .with_retry(fast_retry())
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[tokio::test]
async fn request_shape_is_a_one_token_logprob_completion() {
    let server = MockServer::start().await;
    mount(&server, completion(&[("A", 0.9), ("B", 0.1)], (50, 1))).await;

    let q = Question::noul_with_criteria(
        "Does this convey urgency?",
        "The customer needs help now.",
        "Nothing is time-sensitive.",
    );
    model(&server)
        .ask(json!({"message": "Help! Payouts failing for 3 days."}))
        .question("urgent", q)
        .send()
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].headers.get("authorization").is_none(),
        "no key unless set"
    );
    let body = body_of(&reqs[0]);
    assert_eq!(body["model"], "llama-3.1-8b-instruct");
    assert_eq!(body["max_tokens"], 1);
    assert_eq!(body["temperature"], 0);
    assert_eq!(body["logprobs"], true);
    assert_eq!(body["top_logprobs"], 20);
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
    let prompt = prompt_of(&reqs[0]);
    for needle in [
        "Help! Payouts failing for 3 days.",
        "Does this convey urgency?",
        "A: Yes — The customer needs help now.",
        "B: No — Nothing is time-sensitive.",
        "one of A, B",
    ] {
        assert!(
            prompt.contains(needle),
            "{needle:?} missing from:\n{prompt}"
        );
    }
}

#[tokio::test]
async fn an_api_key_is_sent_only_when_set() {
    let server = MockServer::start().await;
    mount(&server, completion(&[("A", 0.9), ("B", 0.1)], (5, 1))).await;
    model(&server)
        .with_api_key("sk-local")
        .noul("state", "question?")
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs[0].headers.get("authorization").unwrap(),
        "Bearer sk-local"
    );
}

#[tokio::test]
async fn noul_labels_are_trimmed_case_folded_and_summed() {
    let server = MockServer::start().await;
    // " A" and "a" are both yes; "B" no; "The" is not a label.
    mount(
        &server,
        completion(
            &[(" A", 0.5), ("a", 0.2), ("B", 0.2), ("The", 0.1)],
            (40, 1),
        ),
    )
    .await;
    let a = model(&server).noul("state", "question?").await.unwrap();
    assert!(close(a.p_true(), 0.7 / 0.9), "{}", a.p_true());
}

#[tokio::test]
async fn choice_letters_map_to_options_in_order() {
    let server = MockServer::start().await;
    mount(
        &server,
        completion(
            &[("B", 0.6), (" c", 0.3), ("A", 0.05), ("Sure", 0.05)],
            (40, 1),
        ),
    )
    .await;
    let a = model(&server)
        .choice(
            "My card was charged twice.",
            "Which team?",
            ["technical", "billing", "sales"],
        )
        .await
        .unwrap();
    assert_eq!(a.choice(), "billing");
    assert!(close(a.probability("billing"), 0.6 / 0.95));
    assert!(close(a.probability("sales"), 0.3 / 0.95));
    assert!(close(a.probability("technical"), 0.05 / 0.95));
    let order: Vec<&str> = a.probabilities().map(|(o, _)| o).collect();
    assert_eq!(order, ["technical", "billing", "sales"]);

    let prompt = prompt_of(&server.received_requests().await.unwrap()[0]);
    assert!(
        prompt.contains("A: technical\nB: billing\nC: sales"),
        "{prompt}"
    );
}

#[tokio::test]
async fn score_digits_map_to_levels() {
    let server = MockServer::start().await;
    mount(
        &server,
        completion(&[(" 1", 0.5), ("2", 0.3), ("1", 0.1), ("0", 0.1)], (40, 1)),
    )
    .await;
    let a = model(&server)
        .score(
            "I'm furious!",
            "How frustrated?",
            ["Calm", "Frustrated", "Very angry"],
        )
        .await
        .unwrap();
    assert_eq!(a.level(), 1);
    let p = a.probabilities();
    assert!(
        close(p[0], 0.1) && close(p[1], 0.6) && close(p[2], 0.3),
        "{p:?}"
    );
    assert!(close(a.score(), 0.6 + 2.0 * 0.3));
    assert_eq!(a.legend(), ["Calm", "Frustrated", "Very angry"]);
    let prompt = prompt_of(&server.received_requests().await.unwrap()[0]);
    assert!(
        prompt.contains("0: Calm\n1: Frustrated\n2: Very angry"),
        "{prompt}"
    );
}

#[tokio::test]
async fn a_missing_label_is_bounded_not_zero() {
    let server = MockServer::start().await;
    // C never appears in the top K: it gets min(smallest reported 0.1,
    // 1 - reported 0.9) = 0.1, not 0.
    mount(
        &server,
        completion(&[("A", 0.6), ("B", 0.2), ("I", 0.1)], (10, 1)),
    )
    .await;
    let a = model(&server)
        .choice("s", "which?", ["x", "y", "z"])
        .await
        .unwrap();
    assert!(close(a.probability("x"), 0.6 / 0.9));
    assert!(close(a.probability("y"), 0.2 / 0.9));
    assert!(close(a.probability("z"), 0.1 / 0.9));
}

#[tokio::test]
async fn too_little_label_mass_is_a_bad_response() {
    // A thinking model: `<think>` takes nearly all the first token.
    let server = MockServer::start().await;
    mount(
        &server,
        completion(
            &[
                ("<think>", (-0.0001f64).exp()),
                ("A", (-12.0f64).exp()),
                ("B", (-14.0f64).exp()),
            ],
            (10, 1),
        ),
    )
    .await;
    let e = model(&server).noul("s", "q?").await.unwrap_err();
    assert!(matches!(e, DecisionError::BadResponse(_)), "{e:?}");
    assert!(e.to_string().contains("labels cover only"), "{e}");

    // A model answering in words.
    let server = MockServer::start().await;
    mount(
        &server,
        completion(&[("No", 0.9), ("A", 0.05), ("B", 0.01)], (10, 1)),
    )
    .await;
    let e = model(&server).noul("s", "q?").await.unwrap_err();
    assert!(e.to_string().contains("labels cover only"), "{e}");
    // ... accepted only when the floor is lowered on purpose.
    let a = model_with(&server, |b| b.with_min_label_mass(0.05))
        .noul("s", "q?")
        .await
        .unwrap();
    assert!(a.p_true() > 0.8);

    // Positive control: the labels dominate.
    let server = MockServer::start().await;
    mount(
        &server,
        completion(&[("B", 0.85), ("No", 0.1), ("A", 0.05)], (10, 1)),
    )
    .await;
    let a = model(&server).noul("s", "q?").await.unwrap();
    assert!(close(a.p_true(), 0.05 / 0.9), "{}", a.p_true());
}

#[tokio::test]
async fn thinking_can_be_disabled_and_extra_fields_cannot_override_the_core() {
    let server = MockServer::start().await;
    mount(&server, completion(&[("A", 0.9), ("B", 0.1)], (10, 1))).await;
    model_with(&server, |b| {
        b.with_thinking_disabled()
            .with_extra_body(json!({"max_tokens": 500, "reasoning_effort": "none"}))
    })
    .noul("s", "q?")
    .await
    .unwrap();
    // Positive control: without it, nothing extra is sent.
    model(&server).noul("s", "q?").await.unwrap();
    let reqs = server.received_requests().await.unwrap();
    let with = body_of(&reqs[0]);
    assert_eq!(
        with["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    assert_eq!(with["reasoning_effort"], "none");
    assert_eq!(with["max_tokens"], 1, "the core field wins");
    let without = body_of(&reqs[1]);
    assert!(without.get("chat_template_kwargs").is_none());
}

#[tokio::test]
async fn no_label_in_the_top_k_is_a_bad_response() {
    let server = MockServer::start().await;
    mount(&server, completion(&[("Yes", 0.7), ("No", 0.3)], (10, 1))).await;
    let e = model(&server).noul("s", "q?").await.unwrap_err();
    assert!(matches!(e, DecisionError::BadResponse(_)), "{e:?}");
    assert!(e.to_string().contains("none of the labels"), "{e}");

    // A response without logprobs at all.
    let server = MockServer::start().await;
    mount(
        &server,
        json!({"choices": [{"message": {"content": "A"}}], "usage": {"prompt_tokens": 1}}),
    )
    .await;
    let e = model(&server).noul("s", "q?").await.unwrap_err();
    assert!(matches!(e, DecisionError::BadResponse(_)), "{e:?}");

    // Positive control: the same server shape with the labels parses.
    let server = MockServer::start().await;
    mount(
        &server,
        completion(&[("A", 0.6), ("B", 0.3), ("Yes", 0.1)], (10, 1)),
    )
    .await;
    let a = model(&server).noul("s", "q?").await.unwrap();
    assert!(close(a.p_true(), 2.0 / 3.0));
}

#[tokio::test]
async fn temperature_scaling_moves_confidence() {
    let server = MockServer::start().await;
    mount(&server, completion(&[("A", 0.9), ("B", 0.1)], (10, 1))).await;
    let base = model(&server).noul("s", "q?").await.unwrap();
    let soft = model_with(&server, |b| b.with_temperature(2.0))
        .noul("s", "q?")
        .await
        .unwrap();
    let sharp = model_with(&server, |b| b.with_temperature(0.5))
        .noul("s", "q?")
        .await
        .unwrap();
    assert!(close(base.p_true(), 0.9));
    // 0.9^(1/2) / (0.9^(1/2) + 0.1^(1/2)) = 0.75
    assert!(close(soft.p_true(), 0.75), "{}", soft.p_true());
    // 0.81 / 0.82
    assert!(close(sharp.p_true(), 0.81 / 0.82), "{}", sharp.p_true());
    assert!(soft.confidence() < base.confidence() && base.confidence() < sharp.confidence());
}

#[test]
#[should_panic(expected = "finite and positive")]
fn temperature_must_be_positive() {
    let _ = LogprobBackend::new("http://localhost:1").with_temperature(0.0);
}

#[tokio::test]
async fn the_choice_option_limit_follows_the_capabilities() {
    let options: Vec<String> = (0..21).map(|i| format!("opt{i}")).collect();

    // Default 20: 21 options are invalid, and nothing is sent.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion(&[("A", 1.0)], (1, 1))))
        .expect(0)
        .mount(&server)
        .await;
    let m = model(&server);
    assert_eq!(m.capabilities().max_choice_options, 20);
    let e = m.choice("s", "which?", options.clone()).await.unwrap_err();
    assert!(matches!(e, DecisionError::Invalid(_)), "{e:?}");
    drop(server);

    // Raised to 26: sent, asking for enough top_logprobs to see every label.
    let server = MockServer::start().await;
    mount(&server, completion(&[("U", 0.9), ("A", 0.1)], (1, 1))).await;
    let a = model_with(&server, |b| b.with_max_choice_options(26))
        .choice("s", "which?", options)
        .await
        .unwrap();
    assert_eq!(a.choice(), "opt20", "U is the 21st letter");
    let body = body_of(&server.received_requests().await.unwrap()[0]);
    assert_eq!(body["top_logprobs"], 21);
}

#[test]
#[should_panic(expected = "2..=26")]
fn the_choice_limit_cannot_exceed_the_letters() {
    let _ = LogprobBackend::new("http://localhost:1").with_max_choice_options(27);
}

#[tokio::test]
async fn questions_fan_out_concurrently_and_merge_in_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT))
        .respond_with(|req: &WireRequest| {
            // Delays 800 / 400 / 0 ms: the answers complete in the REVERSE
            // of request order.
            let prompt = prompt_of(req);
            let (top, delay): (&[(&str, f64)], u64) = if prompt.contains("urgent?") {
                (&[("A", 0.8), ("B", 0.2)], 800)
            } else if prompt.contains("which team?") {
                (&[("B", 0.9), ("A", 0.1)], 400)
            } else {
                (&[("2", 0.7), ("0", 0.3)], 0)
            };
            ResponseTemplate::new(200)
                .set_body_json(completion(top, (100, 1)))
                .set_delay(Duration::from_millis(delay))
        })
        .mount(&server)
        .await;

    let start = Instant::now();
    let eval = model(&server)
        .ask("Help! My payouts have been failing for 3 days.")
        .noul("urgent", "urgent?")
        .choice("team", "which team?", ["technical", "billing"])
        .score("mood", "how angry?", ["calm", "annoyed", "furious"])
        .send()
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        3,
        "one per question"
    );
    // Sequential cannot finish before 1200 ms; concurrent takes ~800 ms.
    assert!(
        elapsed >= Duration::from_millis(800) && elapsed < Duration::from_millis(1_200),
        "concurrent: {elapsed:?}"
    );
    let ids: Vec<&str> = eval.answers().map(|(id, _)| id).collect();
    assert_eq!(ids, ["urgent", "team", "mood"], "request order");
    assert!(close(eval.p_true("urgent").unwrap(), 0.8));
    assert_eq!(eval.choice("team").unwrap().choice(), "billing");
    assert_eq!(eval.score("mood").unwrap().level(), 2);
    assert_eq!(eval.model(), "qwen3-8b-q4", "the model the server reported");
    // Usage summed over the three completions.
    assert_eq!(eval.usage().input_tokens, 300);
    assert_eq!(eval.usage().output_tokens, 3);
}

#[tokio::test]
async fn a_loopback_server_is_local_and_free() {
    let server = MockServer::start().await; // 127.0.0.1
    mount(
        &server,
        completion(&[("A", 0.9), ("B", 0.1)], (1_000_000, 1)),
    )
    .await;
    let m = model(&server);
    assert!(m.capabilities().local);
    let eval = m.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.cost_usd(), Some(0.0));

    // With a price set, it is priced like any model.
    let priced = model(&server).with_cost(Some(yoagent::provider::CostConfig::new(0.5, 0.0)));
    let eval = priced.ask("s").noul("q", "q?").send().await.unwrap();
    assert!(close(eval.cost_usd().unwrap(), 0.5));
}

// Unix only: connecting to `0.0.0.0` reaches this machine on Linux and
// macOS, not on Windows (where CI only compiles the tests).
#[cfg(unix)]
#[tokio::test]
async fn a_remote_server_is_not_local_and_unpriced() {
    let m = DecisionModel::logprobs("https://api.example.com/v1", "gpt-x");
    assert!(!m.capabilities().local);
    // `0.0.0.0` is not loopback by the backend's rule, yet reaches the local
    // mock server: an end-to-end "remote" evaluation without a network.
    let server = MockServer::start().await;
    mount(&server, completion(&[("A", 0.9), ("B", 0.1)], (100, 1))).await;
    let port = server.address().port();
    let remote =
        DecisionModel::logprobs(format!("http://0.0.0.0:{port}"), "gpt-x").with_retry(fast_retry());
    assert!(!remote.capabilities().local);
    let eval = remote.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.cost_usd(), None, "unpriced, not $0");
    assert_eq!(eval.usage().input_tokens, 100);
}

#[tokio::test]
async fn rate_limits_are_retried_and_errors_mapped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(&server, completion(&[("A", 0.9), ("B", 0.1)], (1, 1))).await;
    let a = model(&server).noul("s", "q?").await.unwrap();
    assert!(close(a.p_true(), 0.9));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    for (status, check) in [
        (
            422u16,
            (|e: &DecisionError| matches!(e, DecisionError::Invalid(_)))
                as fn(&DecisionError) -> bool,
        ),
        (500, |e| {
            matches!(e, DecisionError::Http { status: 500, .. })
        }),
        (401, |e| {
            matches!(e, DecisionError::Http { status: 401, .. })
        }),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CHAT))
            .respond_with(ResponseTemplate::new(status).set_body_string("nope"))
            .mount(&server)
            .await;
        let e = model(&server).noul("s", "q?").await.unwrap_err();
        assert!(check(&e), "{status}: {e:?}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "{status}: not retried"
        );
    }
}

#[tokio::test]
async fn from_logprob_backend_keeps_the_conveniences() {
    let server = MockServer::start().await;
    mount(&server, completion(&[("A", 0.9), ("B", 0.1)], (1_000, 1))).await;
    let m = model_with(&server, |b| b.with_temperature(1.5)).with_api_key("sk-local");
    assert!(m.capabilities().local);
    let eval = m.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.cost_usd(), Some(0.0), "loopback stays $0");
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs[0].headers.get("authorization").unwrap(),
        "Bearer sk-local"
    );
}

#[tokio::test]
async fn a_malformed_base_url_is_invalid_not_retried() {
    let m = DecisionModel::logprobs("not a url", "m").with_retry(RetryConfig {
        max_retries: 3,
        initial_delay_ms: 2_000,
        backoff_multiplier: 1.0,
        max_delay_ms: 2_000,
    });
    let start = Instant::now();
    let e = m.noul("s", "q?").await.unwrap_err();
    assert!(matches!(e, DecisionError::Invalid(_)), "{e:?}");
    assert!(start.elapsed() < Duration::from_secs(1), "not retried");
    // The same for the SystemOne backend.
    let e = DecisionModel::from_backend(SystemOneBackend::new("not a url"), "m")
        .noul("s", "q?")
        .await
        .unwrap_err();
    assert!(matches!(e, DecisionError::Invalid(_)), "{e:?}");
}

// ---------------------------------------------------------------------------
// Spend of split requests: completed questions are recorded when another
// fails or the call times out.
// ---------------------------------------------------------------------------

/// The injection check answers at once; the harmful check fails (or hangs
/// for `slow`); a third check answers at once. Each success reports 100
/// prompt tokens.
async fn split_server(harmful: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT))
        .respond_with(move |req: &WireRequest| {
            if prompt_of(req).contains("clearly harmful") {
                harmful.clone()
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(completion(&[("B", 0.95), ("A", 0.05)], (100, 1)))
            }
        })
        .mount(&server)
        .await;
    server
}

async fn guarded_stats(
    server: &MockServer,
    timeout: Duration,
) -> (Option<String>, yoagent::SessionStats) {
    use yoagent::provider::{MockProvider, ModelConfig};
    let guard = InputGuard::new(model(server))
        .with_check("third", "Is `input` about weather?", 0.9)
        .with_timeout(timeout);
    let mut agent = yoagent::Agent::from_provider(MockProvider::text("hi"), ModelConfig::mock())
        .with_input_guard(guard);
    let mut rx = agent.prompt("hello there").await;
    let (mut rejected, mut stats) = (None, None);
    while let Some(e) = rx.recv().await {
        match e {
            yoagent::AgentEvent::InputRejected { reason } => rejected = Some(reason),
            yoagent::AgentEvent::AgentEnd { stats: s, .. } => stats = Some(s),
            _ => {}
        }
    }
    agent.finish().await;
    (rejected, stats.unwrap())
}

#[tokio::test]
async fn a_failed_question_keeps_the_others_spend() {
    let server = split_server(ResponseTemplate::new(500).set_body_string("boom")).await;
    let (rejected, stats) = guarded_stats(&server, Duration::from_secs(5)).await;
    assert!(rejected.unwrap().contains("could not be screened"));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert_eq!(stats.decision.requests, 1);
    assert_eq!(stats.decision.failures, 1);
    assert_eq!(
        stats.decision.usage.input, 200,
        "the two answered questions"
    );
    assert_eq!(stats.decision.cost_usd, Some(0.0), "loopback: $0");

    // Positive control: all three answer.
    let server = split_server(
        ResponseTemplate::new(200).set_body_json(completion(&[("B", 0.95), ("A", 0.05)], (100, 1))),
    )
    .await;
    let (rejected, stats) = guarded_stats(&server, Duration::from_secs(5)).await;
    assert!(rejected.is_none());
    assert_eq!(stats.decision.failures, 0);
    assert_eq!(stats.decision.usage.input, 300);
}

#[tokio::test]
async fn a_timeout_keeps_the_answered_questions_spend() {
    let slow = ResponseTemplate::new(200)
        .set_body_json(completion(&[("B", 0.95), ("A", 0.05)], (100, 1)))
        .set_delay(Duration::from_secs(5));
    let server = split_server(slow).await;
    let (rejected, stats) = guarded_stats(&server, Duration::from_millis(500)).await;
    assert!(rejected.unwrap().contains("timed out"));
    assert_eq!(stats.decision.timeouts, 1);
    assert_eq!(
        stats.decision.usage.input, 200,
        "the two answered questions"
    );
}

#[tokio::test]
async fn at_most_eight_questions_are_in_flight() {
    // Every response takes 1 s, so how many requests arrive in the first
    // 500 ms is how many were in flight at once.
    let arrivals: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let seen = arrivals.clone();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT))
        .respond_with(move |_req: &WireRequest| {
            seen.lock().unwrap().push(Instant::now());
            ResponseTemplate::new(200)
                .set_body_json(completion(&[("A", 0.9), ("B", 0.1)], (1, 1)))
                .set_delay(Duration::from_millis(1_000))
        })
        .mount(&server)
        .await;
    let m = model(&server);
    let mut ask = m.ask("s");
    for i in 0..12 {
        ask = ask.noul(format!("q{i}"), format!("question {i}?"));
    }
    let start = Instant::now();
    let eval = ask.send().await.unwrap();
    assert_eq!(eval.answers().count(), 12);
    let arrivals = arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 12);
    let early = arrivals
        .iter()
        .filter(|t| t.duration_since(start) < Duration::from_millis(500))
        .count();
    assert_eq!(early, 8, "the cap (8) at once, the rest after a slot frees");
    assert_eq!(model(&server).capabilities().max_concurrent_requests, 8);
}

#[tokio::test]
async fn a_response_without_usage_is_unpriced_unless_free() {
    let server = MockServer::start().await;
    let mut body = completion(&[("A", 0.9), ("B", 0.1)], (0, 0));
    body.as_object_mut().unwrap().remove("usage");
    mount(&server, body).await;

    // Loopback is a free handle: $0 even without usage.
    let eval = model(&server)
        .ask("s")
        .noul("q", "q?")
        .send()
        .await
        .unwrap();
    assert_eq!(eval.cost_usd(), Some(0.0));
    assert_eq!(eval.usage().input_tokens, 0);

    // Priced: no usage means unpriced, never a guessed $0 ...
    let priced = model(&server).with_cost(Some(yoagent::provider::CostConfig::new(1.0, 0.0)));
    let eval = priced.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.cost_usd(), None);

    // ... and the run's stats count it as unpriced.
    let guard = InputGuard::new(priced)
        .without_default_checks()
        .with_check("x", "x?", 0.9);
    let mut agent = yoagent::Agent::from_provider(
        yoagent::provider::MockProvider::text("hi"),
        yoagent::provider::ModelConfig::mock(),
    )
    .with_input_guard(guard);
    let mut rx = agent.prompt("hello").await;
    let mut stats = None;
    while let Some(e) = rx.recv().await {
        if let yoagent::AgentEvent::AgentEnd { stats: s, .. } = e {
            stats = Some(s);
        }
    }
    agent.finish().await;
    let stats = stats.unwrap();
    assert_eq!(stats.decision.requests, 1);
    assert_eq!(stats.decision.unpriced, 1);
    assert_eq!(stats.decision.cost_usd, None);
    assert!(stats.decision.is_unpriced());
}
