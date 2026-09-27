//! Typed answers, confidence, and the [`Evaluation`] that carries them.

use super::error::DecisionError;
use super::question::{Question, QuestionKind};

/// Confidence of a distribution over `n` outcomes:
/// `(n * p_max - 1) / (n - 1)`, clamped to `[0, 1]`.
///
/// This is TypeSafe's published formula for Choice answers
/// (<https://docs.typesafe.ai/confidence>): all mass on one outcome gives
/// 1.0, a uniform spread gives 0.0. yoagent uses it whenever a backend does
/// not report a confidence itself: for Choice (over options), for Score
/// (over levels — TypeSafe derives Score confidence from the same
/// distribution without publishing a separate formula), and for Noul (over
/// yes/no, which reduces to `|2p - 1|`; TypeSafe returns no confidence for
/// Noul, so a Noul answer's confidence is always this computed value unless
/// a backend such as JevK5 reports its own).
///
/// Fewer than two outcomes: 1.0 for one, 0.0 for none. Non-finite inputs are
/// ignored.
pub fn distribution_confidence<I: IntoIterator<Item = f64>>(probabilities: I) -> f64 {
    let mut n = 0usize;
    let mut max = f64::NEG_INFINITY;
    for p in probabilities {
        if p.is_finite() {
            n += 1;
            max = max.max(p);
        }
    }
    match n {
        0 => 0.0,
        1 => 1.0,
        n => ((n as f64 * max - 1.0) / (n as f64 - 1.0)).clamp(0.0, 1.0),
    }
}

/// A yes/no answer.
#[derive(Debug, Clone, PartialEq)]
pub struct NoulAnswer {
    p_true: f64,
    confidence: f64,
}

impl NoulAnswer {
    /// An answer with this probability of yes; confidence computed as
    /// `|2p - 1|` (see [`distribution_confidence`]).
    pub fn new(p_true: f64) -> Self {
        Self {
            p_true,
            confidence: distribution_confidence([p_true, 1.0 - p_true]),
        }
    }

    /// Replace the computed confidence with a backend-reported one.
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }

    /// Probability that the answer is yes, in `[0, 1]`.
    pub fn p_true(&self) -> f64 {
        self.p_true
    }

    /// The backend's confidence, or `|2 * p_true - 1|`.
    pub fn confidence(&self) -> f64 {
        self.confidence
    }
}

/// One option out of a set.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    choice: String,
    /// In option order.
    probabilities: Vec<(String, f64)>,
    confidence: f64,
}

impl ChoiceAnswer {
    /// An answer from a distribution, given in option order. The choice is
    /// its argmax (the first option on a tie) and confidence is computed.
    pub fn new<I, S>(probabilities: I) -> Self
    where
        I: IntoIterator<Item = (S, f64)>,
        S: Into<String>,
    {
        let probabilities: Vec<(String, f64)> = probabilities
            .into_iter()
            .map(|(o, p)| (o.into(), p))
            .collect();
        let mut choice = String::new();
        let mut best = f64::NEG_INFINITY;
        for (option, p) in &probabilities {
            if *p > best {
                best = *p;
                choice = option.clone();
            }
        }
        let confidence = distribution_confidence(probabilities.iter().map(|(_, p)| *p));
        Self {
            choice,
            probabilities,
            confidence,
        }
    }

    /// Replace the argmax with a backend-reported choice.
    pub fn with_choice(mut self, choice: impl Into<String>) -> Self {
        self.choice = choice.into();
        self
    }

    /// Replace the computed confidence with a backend-reported one.
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }

    /// The chosen option.
    pub fn choice(&self) -> &str {
        &self.choice
    }

    /// The backend's confidence, or [`distribution_confidence`] over the
    /// options.
    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    /// Every option with its probability, in option order.
    pub fn probabilities(&self) -> impl Iterator<Item = (&str, f64)> + '_ {
        self.probabilities.iter().map(|(o, p)| (o.as_str(), *p))
    }

    /// Probability of `option` (0.0 when absent).
    pub fn probability(&self, option: &str) -> f64 {
        self.probabilities
            .iter()
            .find(|(o, _)| o == option)
            .map(|(_, p)| *p)
            .unwrap_or(0.0)
    }

    /// Options sorted by probability, highest first (ties keep option order).
    pub fn ranked(&self) -> Vec<(&str, f64)> {
        let mut v: Vec<(&str, f64)> = self.probabilities().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v
    }
}

/// A rating on an ordered scale.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreAnswer {
    score: f64,
    legend: Vec<String>,
    probabilities: Vec<f64>,
    confidence: f64,
}

impl ScoreAnswer {
    /// An answer from per-level probabilities, lowest level first; score
    /// (`sum(i * p_i)`) and confidence computed.
    pub fn new(legend: Vec<String>, probabilities: Vec<f64>) -> Self {
        let score = probabilities
            .iter()
            .enumerate()
            .map(|(i, p)| i as f64 * p)
            .sum();
        let confidence = distribution_confidence(probabilities.iter().copied());
        Self {
            score,
            legend,
            probabilities,
            confidence,
        }
    }

    /// Replace the computed score with a backend-reported one.
    pub fn with_score(mut self, score: f64) -> Self {
        self.score = score;
        self
    }

    /// Replace the computed confidence with a backend-reported one.
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }

    /// Probability-weighted level; can land between levels.
    pub fn score(&self) -> f64 {
        self.score
    }

    /// Level descriptions, lowest first (index = level).
    pub fn legend(&self) -> &[String] {
        &self.legend
    }

    /// Probability of each level, by index.
    pub fn probabilities(&self) -> &[f64] {
        &self.probabilities
    }

    /// The backend's confidence, or [`distribution_confidence`] over the
    /// levels.
    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    /// The most probable level's index (the lowest on a tie).
    pub fn level(&self) -> usize {
        let mut best = 0;
        for (i, p) in self.probabilities.iter().enumerate() {
            if *p > self.probabilities[best] {
                best = i;
            }
        }
        best
    }
}

/// One answer, typed by its question.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Answer {
    /// The answer to a yes/no question.
    Noul(NoulAnswer),
    /// The answer to a choice among options.
    Choice(ChoiceAnswer),
    /// The answer to a rating on ordered levels.
    Score(ScoreAnswer),
}

impl Answer {
    /// The question type this answers.
    pub fn kind(&self) -> QuestionKind {
        match self {
            Self::Noul(_) => QuestionKind::Noul,
            Self::Choice(_) => QuestionKind::Choice,
            Self::Score(_) => QuestionKind::Score,
        }
    }

    /// The confidence of whichever answer this is.
    pub fn confidence(&self) -> f64 {
        match self {
            Self::Noul(a) => a.confidence,
            Self::Choice(a) => a.confidence,
            Self::Score(a) => a.confidence,
        }
    }

    /// The Noul answer, if this is one.
    pub fn as_noul(&self) -> Option<&NoulAnswer> {
        match self {
            Self::Noul(a) => Some(a),
            _ => None,
        }
    }

    /// The Choice answer, if this is one.
    pub fn as_choice(&self) -> Option<&ChoiceAnswer> {
        match self {
            Self::Choice(a) => Some(a),
            _ => None,
        }
    }

    /// The Score answer, if this is one.
    pub fn as_score(&self) -> Option<&ScoreAnswer> {
        match self {
            Self::Score(a) => Some(a),
            _ => None,
        }
    }

    /// Check this answer against the question it answers: its kind, every
    /// probability and confidence finite and in `[0, 1]`, a Choice's options
    /// all among the question's, a Score with one probability and one legend
    /// entry per level. Run by [`DecisionModel`](super::DecisionModel) on
    /// every backend's answers.
    pub(crate) fn validate(&self, id: &str, question: &Question) -> Result<(), DecisionError> {
        let bad = |what: String| Err(DecisionError::BadResponse(format!("answers.{id}: {what}")));
        if self.kind() != question.kind() {
            return bad(format!(
                "a {} answer to a {} question",
                self.kind(),
                question.kind()
            ));
        }
        check_unit(id, "confidence", self.confidence())?;
        match self {
            Self::Noul(a) => check_unit(id, "noul", a.p_true),
            Self::Choice(a) => {
                let options = question.options().unwrap_or_default();
                if !options.contains(&a.choice.as_str()) {
                    return bad(format!("choice {:?} is not one of the options", a.choice));
                }
                let mut seen = std::collections::HashSet::new();
                for (option, p) in &a.probabilities {
                    if !options.contains(&option.as_str()) {
                        return bad(format!(
                            "probability for {option:?}, not one of the options"
                        ));
                    }
                    if !seen.insert(option.as_str()) {
                        return bad(format!("two probabilities for {option:?}"));
                    }
                    check_unit(id, &format!("probabilities.{option}"), *p)?;
                }
                // Every option needs a probability: a partial distribution
                // would inflate the computed confidence (n too small).
                if let Some(missing) = options.iter().find(|o| !seen.contains(**o)) {
                    return bad(format!("no probability for option {missing:?}"));
                }
                check_sum(id, a.probabilities.iter().map(|(_, p)| *p))
            }
            Self::Score(a) => {
                let n = question.levels().map_or(0, <[_]>::len);
                if a.probabilities.len() != n || a.legend.len() != n {
                    return bad(format!(
                        "{} probabilities and {} legend entries for {n} levels",
                        a.probabilities.len(),
                        a.legend.len()
                    ));
                }
                for (i, p) in a.probabilities.iter().enumerate() {
                    check_unit(id, &format!("probabilities[{i}]"), *p)?;
                }
                check_sum(id, a.probabilities.iter().copied())?;
                let top = n.saturating_sub(1) as f64;
                if !(a.score.is_finite() && a.score >= -EPS && a.score <= top + EPS) {
                    return bad(format!("score {} is outside 0..={top}", a.score));
                }
                Ok(())
            }
        }
    }
}

/// Tolerance for float noise at the edges of `[0, 1]`.
const EPS: f64 = 1e-6;

/// How far a distribution may sum from 1: generous enough for servers that
/// round probabilities to a few decimals, tight enough to reject a broken one.
pub(crate) const SUM_TOLERANCE: f64 = 0.02;

fn check_sum(id: &str, probabilities: impl Iterator<Item = f64>) -> Result<(), DecisionError> {
    let sum: f64 = probabilities.sum();
    if (sum - 1.0).abs() <= SUM_TOLERANCE {
        Ok(())
    } else {
        Err(DecisionError::BadResponse(format!(
            "answers.{id}.probabilities: sum to {sum}, not 1"
        )))
    }
}

fn check_unit(id: &str, field: &str, x: f64) -> Result<(), DecisionError> {
    if x.is_finite() && (-EPS..=1.0 + EPS).contains(&x) {
        Ok(())
    } else {
        Err(DecisionError::BadResponse(format!(
            "answers.{id}.{field}: {x} is not in [0, 1]"
        )))
    }
}

impl From<NoulAnswer> for Answer {
    fn from(a: NoulAnswer) -> Self {
        Self::Noul(a)
    }
}

impl From<ChoiceAnswer> for Answer {
    fn from(a: ChoiceAnswer) -> Self {
        Self::Choice(a)
    }
}

impl From<ScoreAnswer> for Answer {
    fn from(a: ScoreAnswer) -> Self {
        Self::Score(a)
    }
}

/// Token usage of one evaluation, as the backend reported it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecisionUsage {
    /// Tokens read: the state and every question (what TypeSafe bills).
    pub input_tokens: u64,
    /// Tokens produced (free on TypeSafe).
    pub output_tokens: u64,
}

impl DecisionUsage {
    /// Usage with these counts.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }

    /// As the crate's [`Usage`](crate::Usage), for [`CostConfig::cost_usd`](crate::provider::CostConfig::cost_usd).
    pub fn to_usage(&self) -> crate::Usage {
        crate::Usage {
            input: self.input_tokens,
            output: self.output_tokens,
            total_tokens: self.input_tokens + self.output_tokens,
            ..Default::default()
        }
    }
}

/// The result of one request: an answer per question id, in request order.
///
/// Built by backends with [`new`](Self::new) and [`with_answer`](Self::with_answer);
/// read through the getters. [`DecisionModel`](super::DecisionModel) keeps
/// only the answers that were asked for, in the order they were asked.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub(crate) model: String,
    pub(crate) answers: Vec<(String, Answer)>,
    pub(crate) usage: DecisionUsage,
    /// Whether the backend reported usage at all. When it did not, the
    /// evaluation cannot be priced.
    pub(crate) usage_reported: bool,
    pub(crate) cost_usd: Option<f64>,
}

impl Evaluation {
    /// An evaluation with no answers yet.
    pub fn new(model: impl Into<String>, usage: DecisionUsage) -> Self {
        Self {
            model: model.into(),
            answers: Vec::new(),
            usage,
            usage_reported: true,
            cost_usd: None,
        }
    }

    /// Add an answer under `id` (replacing an earlier one with that id).
    pub fn with_answer(mut self, id: impl Into<String>, answer: impl Into<Answer>) -> Self {
        let id = id.into();
        let answer = answer.into();
        match self.answers.iter_mut().find(|(k, _)| *k == id) {
            Some(slot) => slot.1 = answer,
            None => self.answers.push((id, answer)),
        }
        self
    }

    /// A cost the backend computed itself. Kept only when the
    /// [`DecisionModel`](super::DecisionModel) is unpriced; a handle with
    /// its own pricing replaces it.
    pub fn with_cost_usd(mut self, cost_usd: Option<f64>) -> Self {
        self.cost_usd = cost_usd;
        self
    }

    /// The model that answered, as the backend reports it — a versioned id
    /// such as `jev-1.13.0` even when you sent an alias. Log it: thresholds
    /// are tuned per model version.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Tokens this request used.
    pub fn usage(&self) -> DecisionUsage {
        self.usage
    }

    /// Cost in USD. `None` = unpriced (never a guessed `0`): an unpriced
    /// model, or a response that reported no usage. A model built with
    /// [`DecisionModel::local`](super::DecisionModel::local) reports
    /// `Some(0.0)`.
    pub fn cost_usd(&self) -> Option<f64> {
        self.cost_usd
    }

    /// Every answer with its id, in request order.
    pub fn answers(&self) -> impl Iterator<Item = (&str, &Answer)> + '_ {
        self.answers.iter().map(|(k, a)| (k.as_str(), a))
    }

    /// The answer under `id`, of any type.
    pub fn get(&self, id: &str) -> Option<&Answer> {
        self.answers.iter().find(|(k, _)| k == id).map(|(_, a)| a)
    }

    /// The Noul answer under `id` (`None` if absent or another type).
    pub fn noul(&self, id: &str) -> Option<&NoulAnswer> {
        self.get(id).and_then(Answer::as_noul)
    }

    /// Shorthand for `noul(id).map(|a| a.p_true())`.
    pub fn p_true(&self, id: &str) -> Option<f64> {
        self.noul(id).map(NoulAnswer::p_true)
    }

    /// The Choice answer under `id`.
    pub fn choice(&self, id: &str) -> Option<&ChoiceAnswer> {
        self.get(id).and_then(Answer::as_choice)
    }

    /// The Score answer under `id`.
    pub fn score(&self, id: &str) -> Option<&ScoreAnswer> {
        self.get(id).and_then(Answer::as_score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_matches_typesafe_formula() {
        // TypeSafe's doc example distribution 0.88/0.12/0.0. The formula
        // gives (3*0.88-1)/2 = 0.82; their example response shows 0.81, a
        // server-side rounding of the same figure.
        let c = distribution_confidence([0.88, 0.12, 0.0]);
        assert!((c - 0.82).abs() < 1e-9, "{c}");
        assert_eq!(distribution_confidence([1.0 / 3.0; 3]), 0.0);
        assert_eq!(distribution_confidence([1.0, 0.0]), 1.0);
        assert_eq!(distribution_confidence([0.2, 0.2, 0.2, 0.2, 0.2]), 0.0);
        assert_eq!(distribution_confidence([0.7]), 1.0);
        assert_eq!(distribution_confidence(Vec::<f64>::new()), 0.0);
    }

    #[test]
    fn noul_confidence_is_distance_from_even() {
        assert!((NoulAnswer::new(0.95).confidence() - 0.9).abs() < 1e-9);
        assert!((NoulAnswer::new(0.05).confidence() - 0.9).abs() < 1e-9);
        assert_eq!(NoulAnswer::new(0.5).confidence(), 0.0);
    }

    #[test]
    fn score_is_probability_weighted() {
        let a = ScoreAnswer::new(
            vec!["Calm".into(), "Frustrated".into(), "Very angry".into()],
            vec![0.0, 0.95, 0.05],
        );
        assert!((a.score() - 1.05).abs() < 1e-9);
        assert_eq!(a.level(), 1);
        assert!((a.confidence() - 0.925).abs() < 1e-9);
    }

    #[test]
    fn choice_keeps_option_order_and_ranks() {
        let a = ChoiceAnswer::new([("technical", 0.12), ("billing", 0.88), ("sales", 0.0)]);
        assert_eq!(a.choice(), "billing");
        let order: Vec<&str> = a.probabilities().map(|(o, _)| o).collect();
        assert_eq!(order, ["technical", "billing", "sales"]);
        assert_eq!(a.ranked()[0], ("billing", 0.88));
        assert_eq!(a.probability("nope"), 0.0);
    }

    #[test]
    fn validation_rejects_malformed_answers() {
        let noul = Question::noul("q?");
        assert!(Answer::from(NoulAnswer::new(0.5))
            .validate("q", &noul)
            .is_ok());
        for bad in [f64::NAN, -0.2, 1.3, f64::INFINITY] {
            assert!(
                Answer::from(NoulAnswer::new(bad))
                    .validate("q", &noul)
                    .is_err(),
                "{bad}"
            );
        }
        assert!(Answer::from(NoulAnswer::new(0.5).with_confidence(f64::NAN))
            .validate("q", &noul)
            .is_err());

        let choice = Question::choice("which?", ["a", "b"]);
        assert!(Answer::from(ChoiceAnswer::new([("a", 0.3), ("b", 0.7)]))
            .validate("q", &choice)
            .is_ok());
        assert!(Answer::from(ChoiceAnswer::new([("a", 0.3), ("c", 0.7)]))
            .validate("q", &choice)
            .is_err());
        assert!(
            Answer::from(ChoiceAnswer::new([("a", 0.3), ("b", 0.7)]).with_choice("z"))
                .validate("q", &choice)
                .is_err()
        );

        let score = Question::score("how?", ["lo", "hi"]);
        assert!(Answer::from(ScoreAnswer::new(
            vec!["lo".into(), "hi".into()],
            vec![0.5, 0.5]
        ))
        .validate("q", &score)
        .is_ok());
        assert!(
            Answer::from(ScoreAnswer::new(vec!["lo".into()], vec![0.5, 0.5]))
                .validate("q", &score)
                .is_err()
        );
        assert!(Answer::from(ScoreAnswer::new(
            vec!["lo".into(), "hi".into(), "x".into()],
            vec![0.2, 0.2, 0.6]
        ))
        .validate("q", &score)
        .is_err());
        // Wrong kind.
        assert!(Answer::from(NoulAnswer::new(0.5))
            .validate("q", &score)
            .is_err());
    }
}
