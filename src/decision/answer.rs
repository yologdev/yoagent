//! Typed answers, confidence, and the [`Evaluation`] that carries them.

use super::question::QuestionKind;
use std::collections::BTreeMap;

/// Confidence of a distribution over `n` outcomes:
/// `(n * p_max - 1) / (n - 1)`, clamped to `[0, 1]`.
///
/// This is TypeSafe's published formula for Choice answers
/// (<https://docs.typesafe.ai/confidence>): all mass on one outcome gives
/// 1.0, a uniform spread gives 0.0. yoagent uses it whenever a backend does
/// not report a confidence itself — for Choice (over options), for Score
/// (over levels; TypeSafe's docs derive Score confidence from the same
/// distribution without publishing a separate formula), and for Noul (over
/// the two outcomes yes/no, which reduces to `|2p - 1|`; TypeSafe's Noul
/// answers carry no confidence of their own).
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
#[non_exhaustive]
pub struct NoulAnswer {
    /// Probability that the answer is yes, in `[0, 1]`.
    pub p_true: f64,
    /// The backend's confidence when it reports one (JevK5 does), otherwise
    /// [`distribution_confidence`] over yes/no: `|2 * p_true - 1|`.
    pub confidence: f64,
}

impl NoulAnswer {
    /// An answer with this probability of yes; confidence computed.
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
}

/// One option out of a set.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ChoiceAnswer {
    /// The highest-probability option.
    pub choice: String,
    /// Every option mapped to its probability (sums to about 1).
    pub probabilities: BTreeMap<String, f64>,
    /// The backend's confidence, or [`distribution_confidence`] over
    /// `probabilities`.
    pub confidence: f64,
}

impl ChoiceAnswer {
    /// An answer from a distribution; the choice is its argmax (the first
    /// option in iteration order on a tie) and confidence is computed.
    pub fn new<I, S>(probabilities: I) -> Self
    where
        I: IntoIterator<Item = (S, f64)>,
        S: Into<String>,
    {
        let mut choice = String::new();
        let mut best = f64::NEG_INFINITY;
        let mut map = BTreeMap::new();
        for (option, p) in probabilities {
            let option = option.into();
            if p > best {
                best = p;
                choice = option.clone();
            }
            map.insert(option, p);
        }
        let confidence = distribution_confidence(map.values().copied());
        Self {
            choice,
            probabilities: map,
            confidence,
        }
    }

    /// Replace the computed confidence with a backend-reported one.
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }

    /// Probability of `option` (0.0 when absent).
    pub fn probability(&self, option: &str) -> f64 {
        self.probabilities.get(option).copied().unwrap_or(0.0)
    }

    /// Options sorted by probability, highest first (ties by name).
    pub fn ranked(&self) -> Vec<(&str, f64)> {
        let mut v: Vec<(&str, f64)> = self
            .probabilities
            .iter()
            .map(|(k, p)| (k.as_str(), *p))
            .collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
        v
    }
}

/// A rating on an ordered scale.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ScoreAnswer {
    /// Probability-weighted level, `sum(i * p_i)`; can land between levels.
    pub score: f64,
    /// Level descriptions, lowest first (index = level).
    pub legend: Vec<String>,
    /// Probability of each level, by index (sums to about 1).
    pub probabilities: Vec<f64>,
    /// The backend's confidence, or [`distribution_confidence`] over the
    /// levels.
    pub confidence: f64,
}

impl ScoreAnswer {
    /// An answer from per-level probabilities; score and confidence
    /// computed.
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

    /// The most probable level's index.
    pub fn level(&self) -> usize {
        self.probabilities
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1).then(b.0.cmp(&a.0)))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
}

/// One answer, typed by its question.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Answer {
    Noul(NoulAnswer),
    Choice(ChoiceAnswer),
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

    pub fn as_noul(&self) -> Option<&NoulAnswer> {
        match self {
            Self::Noul(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_choice(&self) -> Option<&ChoiceAnswer> {
        match self {
            Self::Choice(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_score(&self) -> Option<&ScoreAnswer> {
        match self {
            Self::Score(a) => Some(a),
            _ => None,
        }
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

/// Token usage of one evaluation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl DecisionUsage {
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

/// The result of one request: an answer per question id.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Evaluation {
    /// The model that answered, as the backend reports it — a versioned id
    /// such as `jev-1.13.0` even when you sent an alias. Log it: thresholds
    /// are tuned per model version.
    pub model: String,
    /// Answers by question id.
    pub answers: BTreeMap<String, Answer>,
    /// Tokens this request used.
    pub usage: DecisionUsage,
    /// Cost in USD, when the [`DecisionModel`](super::DecisionModel) knows the
    /// answering model's price. `None` = unpriced (never a guessed `0`); a
    /// local backend reports `Some(0.0)`.
    pub cost_usd: Option<f64>,
}

impl Evaluation {
    /// An evaluation with no answers yet.
    pub fn new(model: impl Into<String>, usage: DecisionUsage) -> Self {
        Self {
            model: model.into(),
            answers: BTreeMap::new(),
            usage,
            cost_usd: None,
        }
    }

    /// Add an answer under `id`.
    pub fn with_answer(mut self, id: impl Into<String>, answer: impl Into<Answer>) -> Self {
        self.answers.insert(id.into(), answer.into());
        self
    }

    /// The answer under `id`, of any type.
    pub fn get(&self, id: &str) -> Option<&Answer> {
        self.answers.get(id)
    }

    /// The Noul answer under `id` (`None` if absent or another type).
    pub fn noul(&self, id: &str) -> Option<&NoulAnswer> {
        self.get(id).and_then(Answer::as_noul)
    }

    /// Shorthand for `noul(id).map(|a| a.p_true)`.
    pub fn p_true(&self, id: &str) -> Option<f64> {
        self.noul(id).map(|a| a.p_true)
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
        // Their doc example: 0.88/0.12/0.0 -> (3*0.88-1)/2 = 0.82.
        let c = distribution_confidence([0.88, 0.12, 0.0]);
        assert!((c - 0.82).abs() < 1e-9, "{c}");
        assert_eq!(distribution_confidence([1.0 / 3.0; 3]), 0.0);
        assert_eq!(distribution_confidence([1.0, 0.0]), 1.0);
        // Clamped.
        assert_eq!(distribution_confidence([0.2, 0.2, 0.2, 0.2, 0.2]), 0.0);
        assert_eq!(distribution_confidence([0.7]), 1.0);
        assert_eq!(distribution_confidence(Vec::<f64>::new()), 0.0);
    }

    #[test]
    fn noul_confidence_is_distance_from_even() {
        assert!((NoulAnswer::new(0.95).confidence - 0.9).abs() < 1e-9);
        assert!((NoulAnswer::new(0.05).confidence - 0.9).abs() < 1e-9);
        assert_eq!(NoulAnswer::new(0.5).confidence, 0.0);
    }

    #[test]
    fn score_is_probability_weighted() {
        let a = ScoreAnswer::new(
            vec!["Calm".into(), "Frustrated".into(), "Very angry".into()],
            vec![0.0, 0.95, 0.05],
        );
        assert!((a.score - 1.05).abs() < 1e-9);
        assert_eq!(a.level(), 1);
        assert!((a.confidence - 0.925).abs() < 1e-9);
    }

    #[test]
    fn choice_argmax_and_ranking() {
        let a = ChoiceAnswer::new([("billing", 0.88), ("technical", 0.12), ("sales", 0.0)]);
        assert_eq!(a.choice, "billing");
        assert_eq!(a.ranked()[1], ("technical", 0.12));
        assert_eq!(a.probability("nope"), 0.0);
    }
}
