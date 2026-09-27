//! Calibration: measure a decision model against labelled examples of your
//! own, and choose thresholds (and a logprob temperature) from the result.

use super::advisory::assert_threshold;
use super::question::{Question, QuestionKind};
use super::{DecisionError, DecisionModel};
use serde_json::Value;
use std::fmt;

/// Reliability bins in a [`CalibrationReport`].
const BINS: usize = 10;
/// The true outcome's rescaled probability is floored at this when
/// computing a log-likelihood, so one confident miss is costly but finite.
const NLL_FLOOR: f64 = 1e-6;
/// Temperatures searched: `1.05^k` for `k` in `-33..=33`, i.e. about 0.2
/// to 5.0 (1.0 included).
const TEMPERATURE_STEPS: i32 = 33;

/// The correct answer to a [`CalibrationExample`]'s question.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Expected {
    /// A Noul's answer: yes (`true`) or no.
    Noul(bool),
    /// A Choice's correct option, by name.
    Choice(String),
    /// A Score's correct level, by index (0 = the lowest).
    Score(usize),
}

/// One labelled example: a state, a question about it, and the answer it
/// should get. Build them with [`noul`](Self::noul), [`choice`](Self::choice)
/// and [`score`](Self::score) (or [`new`](Self::new) for a prepared
/// [`Question`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationExample {
    state: Value,
    question: Question,
    expected: Expected,
}

impl CalibrationExample {
    /// An example from a prepared question. An `expected` that does not fit
    /// the question (another kind, an option it lacks, a level out of range)
    /// is counted as skipped, not evaluated.
    pub fn new(state: impl Into<Value>, question: Question, expected: Expected) -> Self {
        Self {
            state: state.into(),
            question,
            expected,
        }
    }

    /// A yes/no example: the answer to `instructions` about `state` is
    /// `expected`.
    pub fn noul(state: impl Into<Value>, instructions: impl Into<Value>, expected: bool) -> Self {
        Self::new(
            state,
            Question::noul(instructions),
            Expected::Noul(expected),
        )
    }

    /// A choice example: `expected` is the right one of `options`.
    pub fn choice<I, S>(
        state: impl Into<Value>,
        instructions: impl Into<Value>,
        options: I,
        expected: impl Into<String>,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(
            state,
            Question::choice(instructions, options),
            Expected::Choice(expected.into()),
        )
    }

    /// A score example: `expected` is the index of the right level (0 = the
    /// first, lowest).
    pub fn score<I, S>(
        state: impl Into<Value>,
        instructions: impl Into<Value>,
        levels: I,
        expected: usize,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Value>,
    {
        Self::new(
            state,
            Question::score(instructions, levels),
            Expected::Score(expected),
        )
    }

    /// The state evaluated.
    pub fn state(&self) -> &Value {
        &self.state
    }

    /// The question asked.
    pub fn question(&self) -> &Question {
        &self.question
    }

    /// The correct answer.
    pub fn expected(&self) -> &Expected {
        &self.expected
    }

    /// The index of the correct outcome in the answer's distribution, if
    /// `expected` fits the question.
    fn truth(&self) -> Option<usize> {
        match (&self.expected, self.question.kind()) {
            (Expected::Noul(yes), QuestionKind::Noul) => Some(if *yes { 0 } else { 1 }),
            (Expected::Choice(option), QuestionKind::Choice) => self
                .question
                .options()
                .and_then(|o| o.iter().position(|x| x == option)),
            (Expected::Score(level), QuestionKind::Score) => {
                (*level < self.question.levels().map_or(0, <[_]>::len)).then_some(*level)
            }
            _ => None,
        }
    }
}

/// How [`calibrate_with`] runs. Build with [`new`](Self::new) and the
/// `with_*` setters.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationOptions {
    concurrency: usize,
    target_precision: Option<f64>,
}

impl Default for CalibrationOptions {
    fn default() -> Self {
        Self {
            concurrency: 4,
            target_precision: None,
        }
    }
}

impl CalibrationOptions {
    /// The defaults: 4 at a time, no target precision.
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate this many examples at once. Panics on 0.
    pub fn with_concurrency(mut self, n: usize) -> Self {
        assert!(n > 0, "calibration concurrency must be at least 1");
        self.concurrency = n;
        self
    }

    /// Report the lowest Noul threshold reaching this precision. Panics
    /// outside `[0, 1]`.
    pub fn with_target_precision(mut self, p: f64) -> Self {
        assert_threshold("target precision", p);
        self.target_precision = Some(p);
        self
    }

    /// Examples evaluated at once (default 4).
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// The precision the Noul threshold search aims for, if any.
    pub fn target_precision(&self) -> Option<f64> {
        self.target_precision
    }
}

/// An example that failed to evaluate, in [`CalibrationReport::errors`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CalibrationError {
    index: usize,
    error: DecisionError,
}

impl CalibrationError {
    /// The example's position in the input.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Why it failed.
    pub fn error(&self) -> &DecisionError {
        &self.error
    }
}

/// A Noul decision rule "yes when `p_true >= threshold`" and how it did on
/// the examples.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ThresholdPoint {
    /// Say yes at or above this probability.
    pub threshold: f64,
    /// Of the yeses, the fraction that were right.
    pub precision: f64,
    /// Of the true yeses, the fraction found.
    pub recall: f64,
    /// Harmonic mean of precision and recall.
    pub f1: f64,
}

/// One reliability bin: examples whose confidence (the probability of the
/// answer given) fell in `[lower, upper)` (the last bin includes 1.0).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ReliabilityBin {
    /// Lower edge.
    pub lower: f64,
    /// Upper edge.
    pub upper: f64,
    /// Examples in the bin.
    pub count: usize,
    /// Their mean confidence (0 when empty).
    pub mean_confidence: f64,
    /// The fraction of them answered correctly (0 when empty). A calibrated
    /// model's accuracy matches its confidence in every bin.
    pub accuracy: f64,
}

/// What [`calibrate`] measured.
///
/// Every figure is over the examples that were evaluated (`count`);
/// `accuracy`, `brier` and `ece` are `None` when there are none.
/// "Confidence" here is the probability of the answer given (the argmax),
/// so a Noul answered `p_true = 0.3` is a "no" with confidence 0.7.
///
/// Its `Display` is a summary for people, not a stable format; read the
/// fields.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CalibrationReport {
    /// Examples evaluated.
    pub count: usize,
    /// Fraction whose most probable answer was the expected one (a Noul is
    /// "yes" at `p_true >= 0.5`).
    pub accuracy: Option<f64>,
    /// Mean Brier score (lower is better): `(p_true - y)^2` for a Noul
    /// (0 to 1), the sum of squared errors over every option or level for a
    /// Choice or Score (0 to 2). Calibrate one question type at a time for
    /// comparable figures.
    pub brier: Option<f64>,
    /// Expected calibration error over 10 equal-width confidence bins: the
    /// count-weighted mean of `|accuracy - mean confidence|` (0 = perfectly
    /// calibrated).
    pub ece: Option<f64>,
    /// The 10 bins behind `ece`, lowest first — a reliability diagram.
    pub bins: Vec<ReliabilityBin>,
    /// Noul examples only: the threshold on `p_true` maximising F1 (the
    /// lowest on a tie). `None` without Noul examples that are "yes".
    pub best_f1: Option<ThresholdPoint>,
    /// Noul examples only: the lowest threshold whose precision reaches
    /// [`CalibrationOptions::target_precision`]. `None` when no target was
    /// set or none reaches it.
    pub precision_threshold: Option<ThresholdPoint>,
    /// The temperature (about 0.2 to 5.0) that minimises the negative
    /// log-likelihood of the expected answers when every distribution is
    /// rescaled as `p^(1/T)`, normalised. Above 1: the model is
    /// overconfident; below 1: underconfident. Relative to the answers
    /// measured: for a [`logprobs`](DecisionModel::logprobs) model already
    /// at temperature `t0`, use `t0 * suggested` with
    /// [`LogprobBackend::with_temperature`](super::LogprobBackend::with_temperature)
    /// — approximately: the search rescales the answers as they were
    /// reported, after the backend's own normalisation. Other backends have
    /// no such knob; read it as a diagnostic.
    pub suggested_temperature: Option<f64>,
    /// How many evaluated examples each model answered (by
    /// [`Evaluation::model`](super::Evaluation::model)), most first. More
    /// than one entry means the figures mix models — a fallback chain
    /// answered some examples with a fallback; see
    /// [`mixed_models`](Self::mixed_models).
    pub models: Vec<(String, usize)>,
    /// Examples that failed to evaluate. Counted, never fatal.
    pub errors: Vec<CalibrationError>,
    /// Examples not evaluated because `expected` does not fit the question,
    /// by index.
    pub skipped: Vec<usize>,
}

impl CalibrationReport {
    /// Whether more than one model answered — the figures then describe a
    /// mix, not one model.
    pub fn mixed_models(&self) -> bool {
        self.models.len() > 1
    }
}

impl fmt::Display for CalibrationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let num = |x: Option<f64>| x.map_or_else(|| "n/a".to_string(), |x| format!("{x:.3}"));
        write!(
            f,
            "{} evaluated ({} errors, {} skipped): accuracy {}, Brier {}, ECE {}",
            self.count,
            self.errors.len(),
            self.skipped.len(),
            num(self.accuracy),
            num(self.brier),
            num(self.ece)
        )?;
        if self.mixed_models() {
            let list: Vec<String> = self
                .models
                .iter()
                .map(|(m, n)| format!("{m} ×{n}"))
                .collect();
            write!(f, "\nmixed models: {}", list.join(", "))?;
        }
        if let Some(t) = self.suggested_temperature {
            write!(f, "\nsuggested temperature {t:.2}")?;
        }
        for (label, point) in [
            ("best-F1 threshold", self.best_f1),
            ("target-precision threshold", self.precision_threshold),
        ] {
            if let Some(p) = point {
                write!(
                    f,
                    "\n{label} {:.3} (precision {:.3}, recall {:.3}, F1 {:.3})",
                    p.threshold, p.precision, p.recall, p.f1
                )?;
            }
        }
        Ok(())
    }
}

/// Measure `model` on labelled `examples` with the default options (4 at a
/// time, no target precision); see [`calibrate_with`].
pub async fn calibrate(
    model: &DecisionModel,
    examples: impl IntoIterator<Item = CalibrationExample>,
) -> CalibrationReport {
    calibrate_with(model, examples, CalibrationOptions::default()).await
}

/// Measure `model` on labelled `examples`: accuracy, Brier score, expected
/// calibration error with its reliability bins, Noul thresholds, and a
/// temperature suggestion. Use it to choose a backend and thresholds for
/// your data — thresholds are per model version.
///
/// Each example is its own request, evaluated `options.concurrency` at a
/// time. Failures and examples whose `expected` does not fit their question
/// are counted in the report, never fatal. Inside an agent run the requests
/// are recorded in its decision stats like any other.
///
/// ```no_run
/// # async fn demo() {
/// use yoagent::decision::{
///     calibrate_with, CalibrationExample, CalibrationOptions, DecisionModel, LogprobBackend,
/// };
///
/// let q = "Does this message ask to delete or overwrite data?";
/// let examples = vec![
///     CalibrationExample::noul("rm -rf build/ and rebuild", q, true),
///     CalibrationExample::noul("list the files in src/", q, false),
///     CalibrationExample::noul("truncate the users table", q, true),
///     CalibrationExample::noul("what does this function return?", q, false),
/// ];
/// let model = DecisionModel::logprobs("http://localhost:8080", "llama-3.1-8b-instruct");
/// let report = calibrate_with(
///     &model,
///     examples,
///     CalibrationOptions::new().with_target_precision(0.95),
/// )
/// .await;
/// println!("{report}");
/// if let Some(t) = report.suggested_temperature {
///     // Logprob models only: rebuild the backend at that temperature.
///     let backend = LogprobBackend::new("http://localhost:8080").with_temperature(t);
///     let model = DecisionModel::from_logprob_backend(backend, "llama-3.1-8b-instruct");
/// #   let _ = model;
/// }
/// # }
/// ```
pub async fn calibrate_with(
    model: &DecisionModel,
    examples: impl IntoIterator<Item = CalibrationExample>,
    options: CalibrationOptions,
) -> CalibrationReport {
    use futures::StreamExt;

    let mut skipped = Vec::new();
    let mut runnable = Vec::new();
    for (i, example) in examples.into_iter().enumerate() {
        match example.truth() {
            Some(truth) => runnable.push((i, example, truth)),
            None => skipped.push(i),
        }
    }

    let outcomes: Vec<(usize, Result<Scored, DecisionError>)> =
        futures::stream::iter(runnable.into_iter().map(|(i, example, truth)| async move {
            let result = model
                .evaluate(
                    example.state.clone(),
                    vec![("q".to_string(), example.question.clone())],
                )
                .await
                .and_then(|eval| scored(&example, &eval, truth));
            (i, result)
        }))
        .buffer_unordered(options.concurrency.max(1))
        .collect()
        .await;

    let mut scored_all = Vec::new();
    let mut errors = Vec::new();
    for (index, outcome) in outcomes {
        match outcome {
            Ok(s) => scored_all.push(s),
            Err(error) => errors.push(CalibrationError { index, error }),
        }
    }
    errors.sort_by_key(|e| e.index);
    report(&scored_all, &options, errors, skipped)
}

/// One evaluated example: the distribution, the true outcome's index, for
/// a Noul `(p_true, expected)`, and the model that answered.
struct Scored {
    probs: Vec<f64>,
    truth: usize,
    noul: Option<(f64, bool)>,
    model: String,
}

fn scored(
    example: &CalibrationExample,
    eval: &super::Evaluation,
    truth: usize,
) -> Result<Scored, DecisionError> {
    let missing = || DecisionError::BadResponse("answers.q: missing".into());
    let model = eval.model().to_string();
    match example.question.kind() {
        QuestionKind::Noul => {
            let p = eval.p_true("q").ok_or_else(missing)?;
            Ok(Scored {
                probs: vec![p, 1.0 - p],
                truth,
                noul: Some((p, truth == 0)),
                model,
            })
        }
        QuestionKind::Choice => {
            let answer = eval.choice("q").ok_or_else(missing)?;
            let options = example.question.options().unwrap_or_default();
            Ok(Scored {
                probs: options.iter().map(|o| answer.probability(o)).collect(),
                truth,
                noul: None,
                model,
            })
        }
        QuestionKind::Score => Ok(Scored {
            probs: eval
                .score("q")
                .ok_or_else(missing)?
                .probabilities()
                .to_vec(),
            truth,
            noul: None,
            model,
        }),
    }
}

/// Index of the largest probability (the first on a tie).
fn argmax(probs: &[f64]) -> usize {
    let mut best = 0;
    for (i, p) in probs.iter().enumerate() {
        if *p > probs[best] {
            best = i;
        }
    }
    best
}

fn report(
    scored: &[Scored],
    options: &CalibrationOptions,
    errors: Vec<CalibrationError>,
    skipped: Vec<usize>,
) -> CalibrationReport {
    let n = scored.len();
    let mut bins: Vec<(usize, f64, usize)> = vec![(0, 0.0, 0); BINS]; // (count, sum conf, correct)
    let mut correct = 0usize;
    let mut brier = 0.0;
    for s in scored {
        let top = argmax(&s.probs);
        let conf = s.probs[top].clamp(0.0, 1.0);
        let right = top == s.truth;
        correct += usize::from(right);
        brier += match s.noul {
            Some((p, yes)) => (p - if yes { 1.0 } else { 0.0 }).powi(2),
            None => s
                .probs
                .iter()
                .enumerate()
                .map(|(k, p)| (p - if k == s.truth { 1.0 } else { 0.0 }).powi(2))
                .sum(),
        };
        let b = ((conf * BINS as f64) as usize).min(BINS - 1);
        bins[b].0 += 1;
        bins[b].1 += conf;
        bins[b].2 += usize::from(right);
    }
    let nf = n as f64;
    let (accuracy, brier) = if n == 0 {
        (None, None)
    } else {
        (Some(correct as f64 / nf), Some(brier / nf))
    };
    let bins: Vec<ReliabilityBin> = bins
        .iter()
        .enumerate()
        .map(|(i, (count, sum, right))| {
            let c = *count as f64;
            ReliabilityBin {
                lower: i as f64 / BINS as f64,
                upper: (i + 1) as f64 / BINS as f64,
                count: *count,
                mean_confidence: if *count == 0 { 0.0 } else { sum / c },
                accuracy: if *count == 0 { 0.0 } else { *right as f64 / c },
            }
        })
        .collect();
    let ece = (n > 0).then(|| {
        bins.iter()
            .map(|b| b.count as f64 / nf * (b.accuracy - b.mean_confidence).abs())
            .sum()
    });

    let mut models: Vec<(String, usize)> = Vec::new();
    for s in scored {
        match models.iter_mut().find(|(m, _)| *m == s.model) {
            Some(entry) => entry.1 += 1,
            None => models.push((s.model.clone(), 1)),
        }
    }
    models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let nouls: Vec<(f64, bool)> = scored.iter().filter_map(|s| s.noul).collect();
    let points = threshold_points(&nouls);
    let best_f1 = points
        .iter()
        .copied()
        .fold(None::<ThresholdPoint>, |best, p| match best {
            Some(b) if b.f1 >= p.f1 => Some(b),
            _ => Some(p),
        });
    let precision_threshold = options
        .target_precision
        .and_then(|target| points.iter().copied().find(|p| p.precision >= target));

    CalibrationReport {
        count: n,
        accuracy,
        brier,
        ece,
        bins,
        best_f1,
        precision_threshold,
        suggested_temperature: suggest_temperature(scored),
        models,
        errors,
        skipped,
    }
}

/// Every threshold worth trying (each distinct `p_true`, ascending) with its
/// precision, recall and F1. Thresholds that say "yes" to nothing correct
/// are left out; empty without a positive example.
fn threshold_points(nouls: &[(f64, bool)]) -> Vec<ThresholdPoint> {
    let positives = nouls.iter().filter(|(_, y)| *y).count();
    if positives == 0 {
        return Vec::new();
    }
    let mut thresholds: Vec<f64> = nouls
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| p.is_finite())
        .collect();
    thresholds.sort_by(f64::total_cmp);
    thresholds.dedup();
    thresholds
        .into_iter()
        .filter_map(|t| {
            let tp = nouls.iter().filter(|(p, y)| *y && *p >= t).count();
            let fp = nouls.iter().filter(|(p, y)| !*y && *p >= t).count();
            if tp == 0 {
                return None;
            }
            let precision = tp as f64 / (tp + fp) as f64;
            let recall = tp as f64 / positives as f64;
            let f1 = 2.0 * precision * recall / (precision + recall);
            Some(ThresholdPoint {
                threshold: t,
                precision,
                recall,
                f1,
            })
        })
        .collect()
}

/// Mean negative log-likelihood of the true outcomes after rescaling every
/// distribution to temperature `t`: `p^(1/t)` renormalised. A zero
/// probability (which some backends report; the logprob backend floors
/// absent labels instead) stays zero, since no temperature can make it
/// non-zero; only the true outcome's rescaled probability is floored, so a
/// confident miss is finite.
fn nll(scored: &[Scored], t: f64) -> f64 {
    let total: f64 = scored
        .iter()
        .map(|s| {
            let logits: Vec<Option<f64>> = s
                .probs
                .iter()
                .map(|p| (*p > 0.0).then(|| p.min(1.0).ln() / t))
                .collect();
            let max = logits
                .iter()
                .flatten()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max);
            if !max.is_finite() {
                return -NLL_FLOOR.ln();
            }
            let sum: f64 = logits.iter().flatten().map(|l| (l - max).exp()).sum();
            let q = logits[s.truth].map_or(0.0, |l| (l - max).exp() / sum);
            -q.max(NLL_FLOOR).ln()
        })
        .sum();
    total / scored.len() as f64
}

/// The grid temperature with the lowest NLL; ties go to the one closest to
/// 1.0.
fn suggest_temperature(scored: &[Scored]) -> Option<f64> {
    if scored.is_empty() {
        return None;
    }
    let mut best = (1.0, nll(scored, 1.0));
    for k in 1..=TEMPERATURE_STEPS {
        for t in [1.05f64.powi(k), 1.05f64.powi(-k)] {
            let loss = nll(scored, t);
            if loss < best.1 - 1e-12 {
                best = (t, loss);
            }
        }
    }
    best.1.is_finite().then_some(best.0)
}
