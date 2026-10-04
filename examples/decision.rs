//! Decision models in one line each.
//!
//! ```text
//! TYPESAFE_API_KEY=... cargo run --example decision --features decision
//! # or Cloudflare's Clef on Workers AI:
//! CLOUDFLARE_ACCOUNT_ID=... CLOUDFLARE_API_TOKEN=... cargo run --example decision --features decision
//! ```
//!
//! Asks TypeSafe's Jev — or Cloudflare's Clef whenever `CLOUDFLARE_ACCOUNT_ID`
//! is set (wrangler and cf users often have it exported: unset it to use Jev;
//! the token comes from `CLOUDFLARE_API_TOKEN` or `CLOUDFLARE_AUTH_TOKEN`) —
//! three typed questions about one message in a single request, then
//! shows how an agent attaches the same model. Swap in
//! `DecisionModel::local("http://localhost:8000")` to keep the state on your
//! machine (a self-hosted SystemOne server). For Clef inside a Cloudflare
//! Worker, see `integrations/yoagent-workers/examples/clef-worker`.

use yoagent::decision::DecisionModel;
use yoagent::provider::ModelConfig;
use yoagent::Agent;

#[tokio::main]
async fn main() {
    // 1. Choose a model. Nothing is sent until you ask it something.
    let model = match std::env::var("CLOUDFLARE_ACCOUNT_ID") {
        Ok(account) if !account.trim().is_empty() => {
            eprintln!("using Clef on Workers AI (CLOUDFLARE_ACCOUNT_ID is set)");
            DecisionModel::clef(account.trim())
        }
        _ => {
            eprintln!("using TypeSafe's Jev");
            DecisionModel::jev()
        }
    };

    // 2. Ask: one batched request, typed answers with probabilities.
    let message = "Help! My payouts have been failing for 3 days.";
    let eval = match model
        .ask(message)
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
        .send()
        .await
    {
        Ok(eval) => eval,
        Err(e) => {
            eprintln!("decision request failed: {e}");
            std::process::exit(1);
        }
    };

    let team = eval.choice("team").expect("asked");
    let mood = eval.score("mood").expect("asked");
    println!("answered by {}", eval.model());
    println!("urgent: {:.2}", eval.p_true("urgent").unwrap_or_default());
    println!(
        "team:   {} (confidence {:.2})",
        team.choice(),
        team.confidence()
    );
    println!(
        "mood:   {:.2} on 0..2 — mostly \"{}\"",
        mood.score(),
        mood.legend()[mood.level()]
    );
    println!(
        "usage:  {} input tokens, cost {}",
        eval.usage().input_tokens,
        eval.cost_usd()
            .map(|c| format!("${c:.8}"))
            .unwrap_or_else(|| "unpriced".into())
    );

    // 3. Attach it to an agent: advisory skill/tool hints only, never
    //    blocking. They need skills or 40+ tools — without either, this adds
    //    nothing and sends nothing.
    let _agent =
        Agent::from_config(ModelConfig::claude_sonnet_5()).with_decision_model(model.clone());

    // The tool gate is a separate, explicit opt-in because it BLOCKS: calls
    // that look destructive and not clearly requested are denied, and it
    // fails closed (a decision-model error or timeout denies the call). It is
    // defence in depth, not a security boundary. Enable it deliberately:
    //
    //     use yoagent::decision::ToolGate;
    //     let _agent = _agent.with_tool_gate(ToolGate::new(model.clone()));
    //
    // The input guard also BLOCKS: it screens each prompt (prompt injection,
    // clearly harmful requests) and rejects hits; it fails closed too.
    //
    //     use yoagent::decision::InputGuard;
    //     let _agent = _agent.with_input_guard(InputGuard::new(model.clone()));

    // 4. Any OpenAI-compatible server that returns logprobs is a decision
    //    model too — llama.cpp's llama-server, vLLM, SGLang, LM Studio. Its
    //    probabilities are only approximately calibrated, and thinking must
    //    be off (the answer is the first token). And `or` chains a fallback:
    //    hosted Jev first, the local server when Jev fails.
    //
    //     let local = DecisionModel::logprobs("http://localhost:8080", "llama-3.1-8b-instruct");
    //     let model = DecisionModel::jev().or(local);
    //
    //     // A reasoning model needs thinking turned off (llama.cpp, vLLM, SGLang):
    //     use yoagent::decision::LogprobBackend;
    //     let qwen = DecisionModel::from_logprob_backend(
    //         LogprobBackend::new("http://localhost:8080").with_thinking_disabled(),
    //         "qwen3-8b",
    //     );
    //
    // 5. Measure a model (and choose thresholds, or a logprob temperature)
    //    on labelled examples of your own:
    //
    //     use yoagent::decision::{calibrate, CalibrationExample};
    //     let q = "Does this convey urgency?";
    //     let report = calibrate(&model, vec![
    //         CalibrationExample::noul("Help! Payouts failing for 3 days.", q, true),
    //         CalibrationExample::noul("Just saying thanks for the update.", q, false),
    //     ])
    //     .await;
    //     println!("{report}"); // accuracy, Brier, ECE, thresholds, temperature
    //     // Apply a suggested temperature on the logprob backend:
    //     //     LogprobBackend::new(url).with_temperature(t)
}
