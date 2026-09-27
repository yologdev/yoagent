//! `decision::calibrate` against scripted models: calibrated and
//! overconfident answers, thresholds, errors and skips, bounded concurrency.

use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use yoagent::decision::*;

const Q: &str = "Is this a yes?";

/// A model answering each Noul with the `p` in its state (and failing when
/// the state says so).
fn echo() -> DecisionModel {
    DecisionModel::from_backend(
        MockBackend::from_fn(|req| {
            if req.state["fail"] == true {
                return Err(DecisionError::http(503, "down"));
            }
            let mut eval = Evaluation::new("echo", DecisionUsage::new(1, 0));
            for (id, q) in &req.questions {
                eval = match q.kind() {
                    QuestionKind::Noul => eval.with_answer(
                        id.clone(),
                        NoulAnswer::new(req.state["p"].as_f64().unwrap()),
                    ),
                    QuestionKind::Choice => {
                        let probs: Vec<f64> =
                            serde_json::from_value(req.state["probs"].clone()).unwrap();
                        let options = q.options().unwrap();
                        eval.with_answer(
                            id.clone(),
                            ChoiceAnswer::new(options.into_iter().zip(probs)),
                        )
                    }
                    _ => {
                        let probs: Vec<f64> =
                            serde_json::from_value(req.state["probs"].clone()).unwrap();
                        let legend = (0..probs.len()).map(|i| i.to_string()).collect();
                        eval.with_answer(id.clone(), ScoreAnswer::new(legend, probs))
                    }
                };
            }
            Ok(eval)
        }),
        "echo",
    )
}

/// `n` Noul examples answered `p`, of which `yes` are truly yes.
fn group(p: f64, n: usize, yes: usize) -> Vec<CalibrationExample> {
    (0..n)
        .map(|i| CalibrationExample::noul(json!({"p": p, "i": i}), Q, i < yes))
        .collect()
}

#[tokio::test]
async fn a_calibrated_model_has_near_zero_ece() {
    let mut examples = Vec::new();
    for (p, yes) in [(0.1, 1), (0.3, 3), (0.7, 7), (0.9, 9)] {
        examples.extend(group(p, 10, yes));
    }
    let report = calibrate(&echo(), examples).await;
    assert_eq!(report.count, 40);
    assert!(report.ece.unwrap() < 1e-9, "{report}");
    assert!((report.accuracy.unwrap() - 0.8).abs() < 1e-9, "{report}");
    // Mean (p - y)^2: 0.09 for the 0.1/0.9 groups, 0.21 for 0.3/0.7.
    assert!((report.brier.unwrap() - 0.15).abs() < 1e-9, "{report}");
    let t = report.suggested_temperature.unwrap();
    assert!((t - 1.0).abs() < 0.06, "calibrated: T = {t}");
    assert_eq!(report.bins.len(), 10);
    assert_eq!(report.bins.iter().map(|b| b.count).sum::<usize>(), 40);
    assert!(report.errors.is_empty() && report.skipped.is_empty());
}

#[tokio::test]
async fn an_overconfident_model_has_high_ece_and_a_temperature_above_one() {
    let mut examples = group(0.99, 10, 7); // says yes at 0.99, right 70%
    examples.extend(group(0.01, 10, 3)); // says no at 0.99, right 70%
    let report = calibrate(&echo(), examples).await;
    assert!((report.accuracy.unwrap() - 0.7).abs() < 1e-9);
    assert!((report.ece.unwrap() - 0.29).abs() < 1e-9, "{report}");
    let t = report.suggested_temperature.unwrap();
    assert!(t > 1.5, "overconfident: T = {t}");
    // Everything lands in the top bin.
    assert_eq!(report.bins[9].count, 20);
    assert!((report.bins[9].accuracy - 0.7).abs() < 1e-9);
}

#[tokio::test]
async fn an_underconfident_model_gets_a_temperature_below_one() {
    let mut examples = group(0.6, 10, 10); // right every time at 0.6
    examples.extend(group(0.4, 10, 0));
    let report = calibrate(&echo(), examples).await;
    assert!(report.suggested_temperature.unwrap() < 1.0);
}

#[tokio::test]
async fn noul_thresholds_on_a_separable_set() {
    let mut examples = Vec::new();
    for p in [0.6, 0.8, 0.9] {
        examples.push(CalibrationExample::noul(json!({"p": p}), Q, true));
    }
    for p in [0.1, 0.2, 0.4] {
        examples.push(CalibrationExample::noul(json!({"p": p}), Q, false));
    }
    let report = calibrate_with(
        &echo(),
        examples,
        CalibrationOptions::new().with_target_precision(1.0),
    )
    .await;
    let best = report.best_f1.unwrap();
    assert_eq!(best.threshold, 0.6);
    assert_eq!((best.precision, best.recall, best.f1), (1.0, 1.0, 1.0));
    assert_eq!(report.precision_threshold.unwrap().threshold, 0.6);
}

#[tokio::test]
async fn target_precision_trades_recall() {
    let mut examples = Vec::new();
    for p in [0.6, 0.8, 0.9] {
        examples.push(CalibrationExample::noul(json!({"p": p}), Q, true));
    }
    for p in [0.1, 0.7] {
        examples.push(CalibrationExample::noul(json!({"p": p}), Q, false));
    }
    let report = calibrate_with(
        &echo(),
        examples,
        CalibrationOptions::new().with_target_precision(0.9),
    )
    .await;
    // Best F1 at 0.6 (precision 0.75, recall 1); precision 0.9 needs 0.8.
    assert_eq!(report.best_f1.unwrap().threshold, 0.6);
    let strict = report.precision_threshold.unwrap();
    assert_eq!(strict.threshold, 0.8);
    assert_eq!(strict.precision, 1.0);
    assert!((strict.recall - 2.0 / 3.0).abs() < 1e-9);

    // Unreachable target: none.
    let report = calibrate_with(
        &echo(),
        vec![
            CalibrationExample::noul(json!({"p": 0.9}), Q, false),
            CalibrationExample::noul(json!({"p": 0.5}), Q, true),
        ],
        CalibrationOptions::new().with_target_precision(0.9),
    )
    .await;
    assert!(report.precision_threshold.is_none());
}

#[tokio::test]
async fn choice_and_score_examples_are_scored() {
    let examples = vec![
        CalibrationExample::choice(json!({"probs": [0.8, 0.2]}), "which?", ["a", "b"], "a"),
        CalibrationExample::choice(json!({"probs": [0.8, 0.2]}), "which?", ["a", "b"], "b"),
        CalibrationExample::score(json!({"probs": [0.1, 0.9]}), "how?", ["lo", "hi"], 1),
        CalibrationExample::score(json!({"probs": [0.1, 0.9]}), "how?", ["lo", "hi"], 1),
    ];
    let report = calibrate(&echo(), examples).await;
    assert_eq!(report.count, 4);
    assert!((report.accuracy.unwrap() - 0.75).abs() < 1e-9);
    // Multi-class Brier: 0.08, 1.28, 0.02, 0.02.
    assert!((report.brier.unwrap() - 1.4 / 4.0).abs() < 1e-9, "{report}");
    assert!(report.best_f1.is_none(), "Noul only");
}

#[tokio::test]
async fn errors_and_misfit_examples_are_counted_not_fatal() {
    let examples = vec![
        CalibrationExample::noul(json!({"p": 0.9}), Q, true),
        CalibrationExample::noul(json!({"fail": true}), Q, true),
        // The expected option is not an option.
        CalibrationExample::choice(json!({"probs": [0.5, 0.5]}), "which?", ["a", "b"], "c"),
        // Level out of range.
        CalibrationExample::score(json!({"probs": [0.5, 0.5]}), "how?", ["lo", "hi"], 2),
        // A Noul expectation on a Choice question.
        CalibrationExample::new(
            json!({"p": 0.5}),
            Question::choice("which?", ["a", "b"]),
            Expected::Noul(true),
        ),
        CalibrationExample::noul(json!({"p": 0.2}), Q, false),
    ];
    let report = calibrate(&echo(), examples).await;
    assert_eq!(report.count, 2);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].index(), 1);
    assert!(matches!(
        report.errors[0].error(),
        DecisionError::Http { status: 503, .. }
    ));
    assert_eq!(report.skipped, [2, 3, 4]);
    assert_eq!(report.accuracy.unwrap(), 1.0);
    let text = report.to_string();
    assert!(text.contains("1 errors, 3 skipped"), "{text}");
}

#[tokio::test]
async fn nothing_evaluated_is_none_not_zero() {
    let report = calibrate(&echo(), Vec::new()).await;
    assert_eq!(report.count, 0);
    assert!(report.accuracy.is_none() && report.ece.is_none() && report.brier.is_none());
    assert!(report.suggested_temperature.is_none());
}

/// Counts how many evaluations run at once.
struct Counting {
    now: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl DecisionBackend for Counting {
    fn capabilities(&self) -> Capabilities {
        Capabilities::new(QuestionKind::all())
    }
    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(n, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.now.fetch_sub(1, Ordering::SeqCst);
        Ok(Evaluation::new("c", DecisionUsage::default())
            .with_answer(request.questions[0].0.clone(), NoulAnswer::new(0.5)))
    }
}

#[tokio::test]
async fn concurrency_is_bounded() {
    let max = Arc::new(AtomicUsize::new(0));
    let model = DecisionModel::from_backend(
        Counting {
            now: Arc::default(),
            max: max.clone(),
        },
        "c",
    );
    let report = calibrate_with(
        &model,
        group(0.5, 12, 6),
        CalibrationOptions::new().with_concurrency(3),
    )
    .await;
    assert_eq!(report.count, 12);
    let seen = max.load(Ordering::SeqCst);
    assert!(
        (2..=3).contains(&seen),
        "at most 3 at once, and concurrent: {seen}"
    );
}

#[test]
#[should_panic(expected = "target precision")]
fn target_precision_must_be_a_probability() {
    let _ = CalibrationOptions::new().with_target_precision(1.5);
}

#[test]
fn options_have_getters() {
    let o = CalibrationOptions::new();
    assert_eq!((o.concurrency(), o.target_precision()), (4, None));
    let o = o.with_concurrency(2).with_target_precision(0.9);
    assert_eq!((o.concurrency(), o.target_precision()), (2, Some(0.9)));
}

#[tokio::test]
async fn a_zero_probability_miss_does_not_push_the_temperature_up() {
    // Calibrated on the two options it uses ...
    let choice = |truth: &str| {
        CalibrationExample::choice(
            json!({"probs": [0.8, 0.2, 0.0]}),
            "which?",
            ["a", "b", "c"],
            truth,
        )
    };
    let mut examples: Vec<CalibrationExample> = Vec::new();
    for i in 0..20 {
        examples.push(choice(if i < 16 { "a" } else { "b" }));
    }
    let base = calibrate(&echo(), examples.clone()).await;
    let t0 = base.suggested_temperature.unwrap();
    assert!((t0 - 1.0).abs() < 0.06, "{t0}");
    // ... plus one miss on an option it gave exactly 0: rescaling cannot
    // make a zero non-zero, so the miss does not argue for a hotter T.
    examples.push(choice("c"));
    let t = calibrate(&echo(), examples)
        .await
        .suggested_temperature
        .unwrap();
    assert!((t - 1.0).abs() < 0.06, "{t}");
}

#[tokio::test]
async fn a_chain_that_mixes_models_is_visible() {
    let primary = DecisionModel::from_backend(
        MockBackend::from_fn(|req| {
            if req.state["p"].as_f64() == Some(0.9) {
                return Err(DecisionError::http(503, "down"));
            }
            Ok(
                Evaluation::new("primary-1", DecisionUsage::default()).with_answer(
                    req.questions[0].0.clone(),
                    NoulAnswer::new(req.state["p"].as_f64().unwrap()),
                ),
            )
        }),
        "primary",
    );
    let chain = primary.or(echo());
    let mut examples = group(0.1, 3, 0);
    examples.extend(group(0.9, 2, 2));
    let report = calibrate(&chain, examples).await;
    assert!(report.mixed_models());
    assert_eq!(
        report.models,
        [("primary-1".to_string(), 3), ("echo".to_string(), 2)]
    );
    assert!(report.to_string().contains("mixed models"), "{report}");

    // Positive control: one model, not mixed.
    let report = calibrate(&echo(), group(0.1, 3, 0)).await;
    assert!(!report.mixed_models());
    assert_eq!(report.models, [("echo".to_string(), 3)]);
}

#[tokio::test]
async fn best_f1_ties_go_to_the_lowest_threshold() {
    // Yes at 0.9 and 0.2, no at 0.3 and 0.5: thresholds 0.2 (P 0.5, R 1)
    // and 0.9 (P 1, R 0.5) both reach F1 = 2/3.
    let examples = vec![
        CalibrationExample::noul(json!({"p": 0.9}), Q, true),
        CalibrationExample::noul(json!({"p": 0.2}), Q, true),
        CalibrationExample::noul(json!({"p": 0.3}), Q, false),
        CalibrationExample::noul(json!({"p": 0.5}), Q, false),
    ];
    let best = calibrate(&echo(), examples).await.best_f1.unwrap();
    assert_eq!(best.threshold, 0.2);
    assert!((best.f1 - 2.0 / 3.0).abs() < 1e-12);
}

/// Fails every example, later ones sooner — so failures complete in reverse.
struct FailsInReverse;

#[async_trait::async_trait]
impl DecisionBackend for FailsInReverse {
    fn capabilities(&self) -> Capabilities {
        Capabilities::new(QuestionKind::all())
    }
    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        let i = request.state["i"].as_u64().unwrap();
        tokio::time::sleep(Duration::from_millis((5 - i) * 30)).await;
        Err(DecisionError::http(500, format!("example {i}")))
    }
}

#[tokio::test]
async fn calibration_errors_are_sorted_by_index() {
    let examples: Vec<CalibrationExample> = (0..5)
        .map(|i| CalibrationExample::noul(json!({"i": i}), Q, true))
        .collect();
    let report = calibrate_with(
        &DecisionModel::from_backend(FailsInReverse, "m"),
        examples,
        CalibrationOptions::new().with_concurrency(5),
    )
    .await;
    let order: Vec<usize> = report.errors.iter().map(|e| e.index()).collect();
    assert_eq!(order, [0, 1, 2, 3, 4]);
    assert_eq!(report.count, 0);
}
