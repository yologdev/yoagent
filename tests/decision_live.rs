//! Live check against TypeSafe's hosted Jev. Ignored by default; runs only
//! when `TYPESAFE_API_KEY` is set:
//!
//! ```text
//! TYPESAFE_API_KEY=... cargo test --features decision --test decision_live -- --ignored --nocapture
//! ```
//!
//! One batched request with a Noul, a Choice and a Score. The key is never
//! printed.

use yoagent::decision::*;

fn sums_to_one(ps: impl IntoIterator<Item = f64>) -> bool {
    (ps.into_iter().sum::<f64>() - 1.0).abs() < 0.02
}

fn in_unit(x: f64) -> bool {
    (0.0..=1.0).contains(&x)
}

#[tokio::test]
#[ignore = "live: needs TYPESAFE_API_KEY and sends a request to api.typesafe.ai"]
async fn jev_answers_a_batched_request() {
    let key = std::env::var("TYPESAFE_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        eprintln!("skipped: TYPESAFE_API_KEY is not set");
        return;
    }
    let jev = DecisionModel::jev();
    let eval = jev
        .ask("Help! My payouts have been failing for 3 days and my rent is due tomorrow.")
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
        .unwrap_or_else(|e| panic!("live request failed: {e}"));

    println!(
        "model {} | usage {:?} | cost {:?}",
        eval.model(),
        eval.usage(),
        eval.cost_usd()
    );
    assert!(
        eval.model().starts_with("jev-"),
        "reported model: {}",
        eval.model()
    );
    assert!(eval.usage().input_tokens > 0);

    let urgent = eval.noul("urgent").expect("noul answer");
    assert!(in_unit(urgent.p_true()) && in_unit(urgent.confidence()));
    assert!(urgent.p_true() > 0.5, "clearly urgent: {}", urgent.p_true());

    let team = eval.choice("team").expect("choice answer");
    assert!(["billing", "technical", "sales"].contains(&team.choice()));
    assert!(
        sums_to_one(team.probabilities().map(|(_, p)| p)),
        "{team:?}"
    );
    assert!(in_unit(team.confidence()));

    let mood = eval.score("mood").expect("score answer");
    assert_eq!(mood.probabilities().len(), 3);
    assert!(
        sums_to_one(mood.probabilities().iter().copied()),
        "{mood:?}"
    );
    assert!(in_unit(mood.confidence()));
    assert!((0.0..=2.0).contains(&mood.score()));
    assert_eq!(mood.legend().len(), 3);

    // Priced when the reported version is in the table; never guessed.
    if eval.model() == "jev-1.13.0" {
        let cost = eval.cost_usd().expect("jev-1.13.0 is priced");
        assert!(cost > 0.0 && cost < 0.01, "{cost}");
    }
    println!(
        "urgent {:.2} | team {} ({:.2}) | mood {:.2} ({:.2})",
        urgent.p_true(),
        team.choice(),
        team.confidence(),
        mood.score(),
        mood.confidence()
    );
}
