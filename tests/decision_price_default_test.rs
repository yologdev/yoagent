//! The decision presets are unpriced until the process opts in to prices
//! (0.25), and the same handles are priced right after — their prices are
//! resolved at evaluation time, not at construction.
//!
//! Its own test binary with a single test: it asserts on the process-wide
//! price table before anything installs a layer, and it points the process
//! at a plain-HTTP proxy (`HTTP_PROXY`), so that requests to the presets'
//! own hosts (`http://api.typesafe.ai`, `http://api.openai.com`) reach a
//! wiremock server — the host check that gates their list price stays
//! exactly as in production. `clef(..)` and `gpt_6_luna()` themselves are
//! fixed to `https://`, which a plain proxy cannot answer; the
//! `gpt_6_luna()` pricing path is driven through
//! `from_openai_backend`, which prices exactly as the preset does.
#![cfg(all(feature = "decision", feature = "native"))]

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::decision::{DecisionModel, OpenAiDecisionBackend};
use yoagent::provider::prices::{self, global};
use yoagent::retry::RetryConfig;

#[tokio::test]
async fn decision_presets_are_unpriced_until_the_process_opts_in() {
    // A developer's YOAGENT_PRICES would be an opt-in of its own.
    global::clear_override();
    assert!(!global::pricing_enabled());

    let proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": {"q": {"type": "noul", "noul": 0.25}},
            "usage": {"input_tokens": 1_000_000, "output_tokens": 3}
        })))
        .mount(&proxy)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "gpt-6-luna",
            "answers": [{"type": "predicate", "name": "q", "probability": 0.25}],
            "usage": {"input_tokens": 1_000_000, "output_tokens": 3}
        })))
        .mount(&proxy)
        .await;

    // Read when each client is built: set before building the models.
    for var in ["NO_PROXY", "no_proxy", "ALL_PROXY", "all_proxy"] {
        std::env::remove_var(var);
    }
    std::env::set_var("HTTP_PROXY", proxy.uri());
    std::env::set_var("http_proxy", proxy.uri());
    std::env::set_var("TYPESAFE_BASE_URL", "http://api.typesafe.ai");
    std::env::set_var("TYPESAFE_API_KEY", "test-key");

    let jev = DecisionModel::jev().with_retry(RetryConfig::none());
    let luna = DecisionModel::from_openai_backend(
        OpenAiDecisionBackend::new()
            .with_base_url("http://api.openai.com/v1")
            .with_api_key("test-key")
            .with_retry(RetryConfig::none()),
        "gpt-6-luna",
    );

    // Before the opt-in: answered, with usage, and unpriced.
    let eval = jev.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.model(), "jev-1.13.0");
    assert_eq!(eval.cost_usd(), None, "jev() is unpriced by default");
    let eval = luna.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.model(), "gpt-6-luna");
    assert_eq!(eval.cost_usd(), None, "gpt-6-luna is unpriced by default");
    // Both requests went through the proxy to the presets' own hosts.
    let hosts: Vec<String> = proxy
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.host_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(hosts, ["api.typesafe.ai", "api.openai.com"]);

    // After the opt-in, the same handles are priced at the bundled rates:
    // $0.042 / $0.10 per million input tokens, output free.
    prices::enable_bundled();
    let eval = jev.ask("s").noul("q", "q?").send().await.unwrap();
    let cost = eval.cost_usd().expect("jev() priced after enable_bundled");
    assert!((cost - 0.042).abs() < 1e-12, "{cost}");
    let eval = luna.ask("s").noul("q", "q?").send().await.unwrap();
    let cost = eval
        .cost_usd()
        .expect("gpt-6-luna priced after enable_bundled");
    assert!((cost - 0.10).abs() < 1e-12, "{cost}");

    // And unpriced again once the snapshot is cleared.
    global::clear_bundled();
    let eval = jev.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.cost_usd(), None);
}
