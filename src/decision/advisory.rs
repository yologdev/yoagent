//! Advisory integration: a [`TurnHook`] that adds at most a skill hint and a
//! tool hint to a turn's system prompt. Advisory only — it never blocks,
//! never removes a tool, and on any failure adds nothing.

use super::question::{Question, QuestionKind};
use super::DecisionModel;
use crate::types::{TurnContext, TurnHook};
use serde_json::json;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Duration;

/// Most characters of the user's request sent as state.
const MAX_REQUEST_CHARS: usize = 8_000;
/// Most characters of one skill or tool description sent as a criterion.
const MAX_DESCRIPTION_CHARS: usize = 300;

/// Settings for the advisory features [`Agent::with_decision_model`](crate::Agent::with_decision_model)
/// enables. Every field has a default; override with the `with_*` methods
/// and install with [`Agent::with_decision_advisory`](crate::Agent::with_decision_advisory).
///
/// The thresholds are starting points for Jev 1.13, not calibrated
/// constants: they are per model, so re-check them when the model changes.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Advisory {
    /// The model asked.
    pub model: DecisionModel,
    /// Suggest at most one skill per request (default on; needs skills).
    pub skill_hint: bool,
    /// Minimum probability that the request needs a skill at all (0.3, the
    /// gate TypeSafe's skill-suggestion cookbook uses).
    pub skill_needed_threshold: f64,
    /// Minimum confidence of the skill Choice (0.5).
    pub skill_confidence_threshold: f64,
    /// Name the most relevant tools when the agent has many (default on).
    pub tool_hint: bool,
    /// Tool count from which the tool hint runs (40).
    pub tool_hint_min_tools: usize,
    /// Most tools one hint names (3).
    pub max_tool_hints: usize,
    /// Minimum probability for a tool to be named (0.1).
    pub tool_hint_min_probability: f64,
    /// Time limit for the one request per turn (2 s); on expiry nothing is
    /// added.
    pub timeout: Duration,
}

impl Advisory {
    /// Defaults for `model`.
    pub fn new(model: DecisionModel) -> Self {
        Self {
            model,
            skill_hint: true,
            skill_needed_threshold: 0.3,
            skill_confidence_threshold: 0.5,
            tool_hint: true,
            tool_hint_min_tools: 40,
            max_tool_hints: 3,
            tool_hint_min_probability: 0.1,
            timeout: Duration::from_secs(2),
        }
    }

    pub fn with_skill_hint(mut self, on: bool) -> Self {
        self.skill_hint = on;
        self
    }

    pub fn with_skill_thresholds(mut self, needed: f64, confidence: f64) -> Self {
        self.skill_needed_threshold = needed;
        self.skill_confidence_threshold = confidence;
        self
    }

    pub fn with_tool_hint(mut self, on: bool) -> Self {
        self.tool_hint = on;
        self
    }

    pub fn with_tool_hint_min_tools(mut self, n: usize) -> Self {
        self.tool_hint_min_tools = n;
        self
    }

    pub fn with_max_tool_hints(mut self, n: usize) -> Self {
        self.max_tool_hints = n;
        self
    }

    pub fn with_tool_hint_min_probability(mut self, p: f64) -> Self {
        self.tool_hint_min_probability = p;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// The [`TurnHook`] behind the advisory features.
///
/// Per turn it sends at most **one** request, batching a skill Choice (the
/// skills plus a "none" option), a Noul "does this request need a skill",
/// and — only with at least `tool_hint_min_tools` tools — a tool Choice. The
/// state is the latest user message alone, so the answer (and the line it
/// produces) is stable across the tool-calling turns of one request: the
/// result is memoized on that state, sending nothing on later turns and
/// keeping the system prompt — and the provider's prompt cache — unchanged.
///
/// Nothing is sent when there is no user text, or when there are no skills
/// to suggest and fewer tools than the threshold.
pub struct Advisor {
    advisory: Advisory,
    /// `(name, description)`.
    skills: Vec<(String, String)>,
    memo: Mutex<Option<(u64, Option<String>)>>,
}

impl std::fmt::Debug for Advisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Advisor")
            .field("advisory", &self.advisory)
            .field("skills", &self.skills.len())
            .finish_non_exhaustive()
    }
}

impl Advisor {
    /// An advisor over `skills` (may be empty).
    pub fn new(advisory: Advisory, skills: &crate::skills::SkillSet) -> Self {
        let mut seen = std::collections::HashSet::new();
        let skills = skills
            .skills()
            .iter()
            .filter(|s| seen.insert(s.name.clone()))
            .map(|s| {
                (
                    s.name.clone(),
                    truncate(&s.description, MAX_DESCRIPTION_CHARS),
                )
            })
            .collect();
        Self {
            advisory,
            skills,
            memo: Mutex::new(None),
        }
    }

    /// Whether this advisor could ever send anything with `tools` tools.
    pub(crate) fn could_act(advisory: &Advisory, skills: &crate::skills::SkillSet) -> bool {
        (advisory.skill_hint && !skills.is_empty()) || advisory.tool_hint
    }

    fn none_option(&self) -> &'static str {
        if self.skills.iter().any(|(n, _)| n == "none") {
            "no_skill"
        } else {
            "none"
        }
    }

    async fn advise(&self, turn: &TurnContext<'_>) -> Option<String> {
        let request = turn.latest_user_text()?;
        if request.trim().is_empty() {
            return None;
        }
        let caps = self.advisory.model.capabilities();
        let choice_ok = caps.supports(QuestionKind::Choice);
        let want_skill = self.advisory.skill_hint
            && !self.skills.is_empty()
            && choice_ok
            && caps.supports(QuestionKind::Noul)
            && self.skills.len() < caps.max_choice_options;
        let want_tools = self.advisory.tool_hint
            && choice_ok
            && turn.tools.len() >= self.advisory.tool_hint_min_tools.max(2)
            && turn.tools.len() <= caps.max_choice_options;
        if !want_skill && !want_tools {
            return None;
        }

        let key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            request.hash(&mut h);
            want_skill.hash(&mut h);
            if want_tools {
                for t in turn.tools {
                    t.name.hash(&mut h);
                }
            }
            h.finish()
        };
        if let Some((k, line)) = self.memo.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            if *k == key {
                return line.clone();
            }
        }

        let mut questions = Vec::new();
        let none = self.none_option();
        if want_skill {
            let mut options: Vec<(String, String)> = self.skills.clone();
            options.push((none.into(), "No listed skill fits the request.".into()));
            questions.push((
                "skill".to_string(),
                Question::choice_described(
                    "Which of these skills, if any, is the right one to load to help with \
                     the user's latest request in `request`?",
                    options,
                ),
            ));
            questions.push((
                "skill_needed".to_string(),
                Question::noul(
                    "Would a careful expert handling `request` consult a specific documented \
                     procedure or set of commands, rather than answering from general \
                     understanding?",
                ),
            ));
        }
        if want_tools {
            questions.push((
                "tools".to_string(),
                Question::choice_described(
                    "Which of these tools will the assistant most likely need next to carry \
                     out `request`?",
                    turn.tools.iter().map(|t| {
                        let desc = truncate(&t.description, MAX_DESCRIPTION_CHARS);
                        let desc = if desc.trim().is_empty() {
                            t.name.clone()
                        } else {
                            desc
                        };
                        (t.name.clone(), desc)
                    }),
                ),
            ));
        }

        let state = json!({ "request": truncate(&request, MAX_REQUEST_CHARS) });
        let result = tokio::time::timeout(
            self.advisory.timeout,
            self.advisory.model.evaluate(state, questions),
        )
        .await;
        let eval = match result {
            Ok(Ok(eval)) => eval,
            Ok(Err(e)) => {
                tracing::warn!("decision advisory skipped this request: {e}");
                self.remember(key, None);
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    "decision advisory skipped this request: no answer within {:?}",
                    self.advisory.timeout
                );
                self.remember(key, None);
                return None;
            }
        };

        let mut lines = Vec::new();
        if want_skill {
            let needed = eval.p_true("skill_needed").unwrap_or(0.0);
            if let Some(choice) = eval.choice("skill") {
                if needed >= self.advisory.skill_needed_threshold
                    && choice.choice != none
                    && choice.confidence >= self.advisory.skill_confidence_threshold
                {
                    lines.push(format!(
                        "Relevant to the current request: {}. Ignore this if it does not fit \
                         what the user actually asked for.",
                        choice.choice
                    ));
                }
            }
        }
        if want_tools {
            if let Some(choice) = eval.choice("tools") {
                let named: Vec<&str> = choice
                    .ranked()
                    .into_iter()
                    .filter(|(_, p)| *p >= self.advisory.tool_hint_min_probability)
                    .take(self.advisory.max_tool_hints)
                    .map(|(name, _)| name)
                    .collect();
                if !named.is_empty() {
                    lines.push(format!(
                        "Tools likely relevant to the current request: {}. This is a hint \
                         only; every tool remains available.",
                        named.join(", ")
                    ));
                }
            }
        }
        let line = (!lines.is_empty()).then(|| lines.join("\n"));
        self.remember(key, line.clone());
        line
    }

    fn remember(&self, key: u64, line: Option<String>) {
        *self.memo.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, line));
    }
}

#[async_trait::async_trait]
impl TurnHook for Advisor {
    async fn before_turn(&self, turn: &TurnContext<'_>) -> Option<String> {
        self.advise(turn).await
    }
}

/// At most `max` characters, on a char boundary.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}
