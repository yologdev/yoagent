//! Typed questions and the request that carries them.

use super::backend::Capabilities;
use super::error::DecisionError;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

/// The three question types a decision model answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum QuestionKind {
    /// Yes/no; answered with the probability of yes.
    Noul,
    /// One option out of a set; answered with a distribution over options.
    Choice,
    /// A rating on an ordered scale of 2 to 10 levels.
    Score,
}

impl QuestionKind {
    /// The wire name: `"noul"`, `"choice"` or `"score"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Noul => "noul",
            Self::Choice => "choice",
            Self::Score => "score",
        }
    }

    /// All kinds, in wire order.
    pub fn all() -> &'static [QuestionKind] {
        &[Self::Noul, Self::Choice, Self::Score]
    }
}

impl fmt::Display for QuestionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Body {
    Noul {
        criteria: Option<(Value, Value)>,
    },
    Choice {
        options: Vec<(String, Option<Value>)>,
    },
    Score {
        levels: Vec<Value>,
    },
}

/// One typed question.
///
/// `instructions` (and every criterion) may be a string, an object or an
/// array — an object lets you put the question in one field and the data it
/// refers to in others. Anything convertible into a [`serde_json::Value`]
/// works: `&str`, `String`, `json!({..})`.
///
/// ```
/// use yoagent::decision::Question;
/// let urgent = Question::noul("Does this convey urgency?");
/// let team = Question::choice("Which team should handle this?", ["billing", "technical"]);
/// let mood = Question::score("How frustrated is the customer?", ["Calm", "Frustrated", "Very angry"]);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    instructions: Value,
    body: Body,
}

impl Question {
    /// A yes/no question.
    pub fn noul(instructions: impl Into<Value>) -> Self {
        Self {
            instructions: instructions.into(),
            body: Body::Noul { criteria: None },
        }
    }

    /// A yes/no question with descriptions of what a yes and a no mean.
    pub fn noul_with_criteria(
        instructions: impl Into<Value>,
        if_true: impl Into<Value>,
        if_false: impl Into<Value>,
    ) -> Self {
        Self {
            instructions: instructions.into(),
            body: Body::Noul {
                criteria: Some((if_true.into(), if_false.into())),
            },
        }
    }

    /// A choice among named options (sent with no description, `null`).
    /// Option order is preserved on the wire.
    pub fn choice<I, S>(instructions: impl Into<Value>, options: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            instructions: instructions.into(),
            body: Body::Choice {
                options: options.into_iter().map(|o| (o.into(), None)).collect(),
            },
        }
    }

    /// A choice among options, each with a rubric description (the
    /// question's `criteria`). Option order is preserved on the wire.
    pub fn choice_with_criteria<I, K, D>(instructions: impl Into<Value>, options: I) -> Self
    where
        I: IntoIterator<Item = (K, D)>,
        K: Into<String>,
        D: Into<Value>,
    {
        Self {
            instructions: instructions.into(),
            body: Body::Choice {
                options: options
                    .into_iter()
                    .map(|(k, d)| (k.into(), Some(d.into())))
                    .collect(),
            },
        }
    }

    /// A score over ordered levels, lowest first (2 to 10 levels).
    pub fn score<I, S>(instructions: impl Into<Value>, levels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Value>,
    {
        Self {
            instructions: instructions.into(),
            body: Body::Score {
                levels: levels.into_iter().map(Into::into).collect(),
            },
        }
    }

    /// Which type of question this is.
    pub fn kind(&self) -> QuestionKind {
        match self.body {
            Body::Noul { .. } => QuestionKind::Noul,
            Body::Choice { .. } => QuestionKind::Choice,
            Body::Score { .. } => QuestionKind::Score,
        }
    }

    /// The question text (or structured instructions).
    pub fn instructions(&self) -> &Value {
        &self.instructions
    }

    /// A Choice's option names, in order. `None` for other kinds.
    pub fn options(&self) -> Option<Vec<&str>> {
        match &self.body {
            Body::Choice { options } => Some(options.iter().map(|(o, _)| o.as_str()).collect()),
            _ => None,
        }
    }

    /// A Choice's options with their descriptions (`None` = sent as
    /// `null`), in order. `None` for other kinds.
    pub fn choice_criteria(&self) -> Option<Vec<(&str, Option<&Value>)>> {
        match &self.body {
            Body::Choice { options } => Some(
                options
                    .iter()
                    .map(|(o, d)| (o.as_str(), d.as_ref()))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// A Noul's `(true, false)` criteria, when it has them.
    pub fn noul_criteria(&self) -> Option<(&Value, &Value)> {
        match &self.body {
            Body::Noul {
                criteria: Some((t, f)),
            } => Some((t, f)),
            _ => None,
        }
    }

    /// A Score's level descriptions, lowest first. `None` for other kinds.
    pub fn levels(&self) -> Option<&[Value]> {
        match &self.body {
            Body::Score { levels } => Some(levels),
            _ => None,
        }
    }

    /// Check this question against a backend's limits. `id` names it in the
    /// error.
    pub(crate) fn validate(&self, id: &str, caps: &Capabilities) -> Result<(), DecisionError> {
        let kind = self.kind();
        if !caps.supports(kind) {
            return Err(DecisionError::Unsupported(format!(
                "questions.{id}: {kind} questions are not supported by this backend"
            )));
        }
        check_text_like(&self.instructions, &format!("questions.{id}.instructions"))?;
        match &self.body {
            Body::Noul { criteria } => {
                if let Some((t, f)) = criteria {
                    check_text_like(t, &format!("questions.{id}.criteria.true"))?;
                    check_text_like(f, &format!("questions.{id}.criteria.false"))?;
                }
            }
            Body::Choice { options } => {
                if options.len() < 2 {
                    return Err(DecisionError::Invalid(format!(
                        "questions.{id}.criteria: a choice needs at least 2 options, got {}",
                        options.len()
                    )));
                }
                if options.len() > caps.max_choice_options {
                    return Err(DecisionError::Invalid(format!(
                        "questions.{id}.criteria: {} options exceed the backend's limit of {}",
                        options.len(),
                        caps.max_choice_options
                    )));
                }
                let mut seen = HashSet::new();
                for (name, desc) in options {
                    if name.trim().is_empty() {
                        return Err(DecisionError::Invalid(format!(
                            "questions.{id}.criteria: an option name is empty"
                        )));
                    }
                    if !seen.insert(name.as_str()) {
                        return Err(DecisionError::Invalid(format!(
                            "questions.{id}.criteria: duplicate option {name:?}"
                        )));
                    }
                    if let Some(d) = desc {
                        check_text_like(d, &format!("questions.{id}.criteria.{name}"))?;
                    }
                }
            }
            Body::Score { levels } => {
                if levels.len() < 2 || levels.len() > caps.max_score_levels {
                    return Err(DecisionError::Invalid(format!(
                        "questions.{id}.criteria: a score needs 2 to {} levels, got {}",
                        caps.max_score_levels,
                        levels.len()
                    )));
                }
                for (i, level) in levels.iter().enumerate() {
                    check_text_like(level, &format!("questions.{id}.criteria[{i}]"))?;
                }
            }
        }
        Ok(())
    }
}

/// Instructions, criteria and state must be a non-empty string, an object or
/// an array.
fn check_text_like(v: &Value, field: &str) -> Result<(), DecisionError> {
    match v {
        Value::String(s) if s.trim().is_empty() => Err(DecisionError::Invalid(format!(
            "{field}: must not be empty"
        ))),
        Value::String(_) | Value::Object(_) | Value::Array(_) => Ok(()),
        other => Err(DecisionError::Invalid(format!(
            "{field}: must be a string, object or array, got {}",
            json_type(other)
        ))),
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Serialized in insertion order (a `serde_json::Value` map would sort the
/// options alphabetically).
impl Serialize for Question {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let has_criteria = !matches!(self.body, Body::Noul { criteria: None });
        let mut map = serializer.serialize_map(Some(if has_criteria { 3 } else { 2 }))?;
        map.serialize_entry("type", self.kind().as_str())?;
        map.serialize_entry("instructions", &self.instructions)?;
        match &self.body {
            Body::Noul { criteria: None } => {}
            Body::Noul {
                criteria: Some((t, f)),
            } => map.serialize_entry("criteria", &NoulCriteria(t, f))?,
            Body::Choice { options } => {
                map.serialize_entry("criteria", &ChoiceCriteria(options))?
            }
            Body::Score { levels } => map.serialize_entry("criteria", levels)?,
        }
        map.end()
    }
}

struct NoulCriteria<'a>(&'a Value, &'a Value);

impl Serialize for NoulCriteria<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("true", self.0)?;
        map.serialize_entry("false", self.1)?;
        map.end()
    }
}

struct ChoiceCriteria<'a>(&'a [(String, Option<Value>)]);

impl Serialize for ChoiceCriteria<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, desc) in self.0 {
            map.serialize_entry(name, desc)?;
        }
        map.end()
    }
}

/// What a backend is asked to evaluate: one `state` and any number of named
/// questions about it.
///
/// Usually built for you by [`DecisionModel`](super::DecisionModel); build one
/// directly when implementing or testing a [`DecisionBackend`](super::DecisionBackend).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Request {
    /// The model id or alias sent in the `model` field.
    pub model: String,
    /// The content to evaluate: a string, an object or an array.
    pub state: Value,
    /// `(id, question)` pairs. Ids are yours; answers come back under them.
    pub questions: Vec<(String, Question)>,
}

impl Request {
    /// A request with no questions yet.
    pub fn new(model: impl Into<String>, state: impl Into<Value>) -> Self {
        Self {
            model: model.into(),
            state: state.into(),
            questions: Vec::new(),
        }
    }

    /// Add a question under `id`.
    pub fn question(mut self, id: impl Into<String>, question: Question) -> Self {
        self.questions.push((id.into(), question));
        self
    }

    /// The question asked under `id`.
    pub fn get(&self, id: &str) -> Option<&Question> {
        self.questions
            .iter()
            .find(|(qid, _)| qid == id)
            .map(|(_, q)| q)
    }

    /// Check the request against a backend's limits before sending it.
    ///
    /// The token limits are checked on an estimate (4 bytes per token, the
    /// same heuristic as [`context::estimate_tokens`](crate::context::estimate_tokens)),
    /// so a request near a limit may still be rejected by the server with 422.
    pub(crate) fn validate(&self, caps: &Capabilities) -> Result<(), DecisionError> {
        if self.model.trim().is_empty() {
            return Err(DecisionError::Invalid("model: must not be empty".into()));
        }
        check_text_like(&self.state, "state")?;
        if self.questions.is_empty() {
            return Err(DecisionError::Invalid(
                "questions: at least one question is required".into(),
            ));
        }
        let mut seen = HashSet::new();
        for (id, q) in &self.questions {
            if id.trim().is_empty() {
                return Err(DecisionError::Invalid(
                    "questions: a question id must not be empty".into(),
                ));
            }
            if !seen.insert(id.as_str()) {
                return Err(DecisionError::Invalid(format!(
                    "questions.{id}: duplicate question id"
                )));
            }
            q.validate(id, caps)?;
        }
        let state_tokens = estimate_json_tokens(&self.state);
        let question_tokens: Vec<usize> = self
            .questions
            .iter()
            .map(|(_, q)| estimate_json_tokens(q))
            .collect();
        if let Some(max) = caps.max_request_tokens {
            let total = state_tokens + question_tokens.iter().sum::<usize>();
            if total > max {
                return Err(DecisionError::Invalid(format!(
                    "request: about {total} tokens (estimated) exceeds the backend's \
                     {max}-token request limit; trim the state or split the questions"
                )));
            }
        }
        if let Some(max) = caps.max_state_and_question_tokens {
            let longest = question_tokens.iter().copied().max().unwrap_or(0);
            if state_tokens + longest > max {
                return Err(DecisionError::Invalid(format!(
                    "state: about {} tokens (estimated) for the state plus the longest \
                     question exceeds the backend's {max}-token limit",
                    state_tokens + longest
                )));
            }
        }
        Ok(())
    }
}

fn estimate_json_tokens<T: Serialize>(v: &T) -> usize {
    let len = match serde_json::to_vec(v) {
        Ok(bytes) => bytes.len(),
        Err(_) => 0,
    };
    len.div_ceil(4)
}

/// Wire order: `state`, `model`, `questions` (in insertion order).
impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("state", &self.state)?;
        map.serialize_entry("model", &self.model)?;
        map.serialize_entry("questions", &Questions(&self.questions))?;
        map.end()
    }
}

struct Questions<'a>(&'a [(String, Question)]);

impl Serialize for Questions<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (id, q) in self.0 {
            map.serialize_entry(id, q)?;
        }
        map.end()
    }
}
