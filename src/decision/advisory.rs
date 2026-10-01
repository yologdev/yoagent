//! Advisory integration: a [`TurnHook`] that adds at most a skill hint and a
//! tool hint to a turn's latest user message. Advisory only — it never
//! blocks, never removes a tool, and on any failure adds nothing.

use super::question::{Question, QuestionKind};
use super::{DecisionError, DecisionModel};
use crate::types::{TurnContext, TurnHook};
use serde_json::json;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Most characters of the user's request sent as state.
const MAX_REQUEST_CHARS: usize = 8_000;
/// Most characters of one skill or tool description sent as a criterion.
const MAX_DESCRIPTION_CHARS: usize = 300;

/// Panic unless `p` is a probability — a threshold outside `[0, 1]` is a
/// programmer error, and failing at setup beats silently never (or always)
/// firing.
pub(crate) fn assert_threshold(name: &str, p: f64) {
    assert!(
        (0.0..=1.0).contains(&p),
        "{name} must be a probability in [0, 1], got {p}"
    );
}

/// Settings for the advisory features
/// [`Agent::with_decision_model`](crate::Agent::with_decision_model)
/// enables. Every setting has a default; override with the `with_*` methods
/// and install with
/// [`Agent::with_decision_advisory`](crate::Agent::with_decision_advisory).
///
/// Defaults: skill hint on (needs skills), shown when the "is a skill
/// needed" Noul is at least 0.3 and the skill Choice's confidence at least
/// 0.5; tool hint on from 40 tools, naming at most 3 tools with probability
/// at least 0.1; 2 s per request. These are starting points for Jev 1.13,
/// not calibrated constants — thresholds are per model.
#[derive(Debug, Clone)]
pub struct Advisory {
    pub(crate) model: DecisionModel,
    skill_hint: bool,
    skill_need_threshold: f64,
    skill_confidence_threshold: f64,
    tool_hint: bool,
    tool_hint_min_tools: usize,
    max_tool_hints: usize,
    tool_hint_min_probability: f64,
    timeout: Duration,
}

impl Advisory {
    /// Defaults for `model`.
    pub fn new(model: DecisionModel) -> Self {
        Self {
            model,
            skill_hint: true,
            skill_need_threshold: 0.3,
            skill_confidence_threshold: 0.5,
            tool_hint: true,
            tool_hint_min_tools: 40,
            max_tool_hints: 3,
            tool_hint_min_probability: 0.1,
            timeout: Duration::from_secs(2),
        }
    }

    /// Suggest at most one skill per request (default on; needs skills).
    pub fn with_skill_hint(mut self, on: bool) -> Self {
        self.skill_hint = on;
        self
    }

    /// Minimum probability that the request needs a skill at all (0.3 —
    /// adapted from TypeSafe's skill-suggestion cookbook, where 0.3 applies
    /// to the mean of three gate questions; here to one). Panics outside
    /// `[0, 1]`.
    pub fn with_skill_need_threshold(mut self, p: f64) -> Self {
        assert_threshold("skill need threshold", p);
        self.skill_need_threshold = p;
        self
    }

    /// Minimum confidence of the skill Choice (0.5, yoagent's own default).
    /// Panics outside `[0, 1]`.
    pub fn with_skill_confidence_threshold(mut self, p: f64) -> Self {
        assert_threshold("skill confidence threshold", p);
        self.skill_confidence_threshold = p;
        self
    }

    /// Name the most relevant tools when the agent has many (default on).
    pub fn with_tool_hint(mut self, on: bool) -> Self {
        self.tool_hint = on;
        self
    }

    /// Tool count from which the tool hint runs (40).
    pub fn with_tool_hint_min_tools(mut self, n: usize) -> Self {
        self.tool_hint_min_tools = n;
        self
    }

    /// Most tools one hint names (3).
    pub fn with_max_tool_hints(mut self, n: usize) -> Self {
        self.max_tool_hints = n;
        self
    }

    /// Minimum probability for a tool to be named (0.1). Panics outside
    /// `[0, 1]`.
    pub fn with_tool_hint_min_probability(mut self, p: f64) -> Self {
        assert_threshold("tool hint minimum probability", p);
        self.tool_hint_min_probability = p;
        self
    }

    /// Time limit for the one request per user message (2 s); on expiry
    /// nothing is added.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// The [`TurnHook`] behind the advisory features.
///
/// Per user message it sends at most **one** request, batching a skill
/// Choice (the skills plus a "none" option), a Noul "does this request need
/// a skill", and — only with at least `tool_hint_min_tools` tools — a tool
/// Choice. The state is the user's request (see
/// [`TurnContext::user_request`]); the result is memoized on it, so the
/// tool-calling turns of one request send nothing more and see the same
/// note.
pub(crate) struct Advisor {
    advisory: Advisory,
    /// The model with the advisory timeout applied, so a timeout is reported
    /// (and counted) as one.
    model: DecisionModel,
    /// `(name, description)`.
    skills: Vec<(String, String)>,
    memo: Mutex<Option<(u64, Option<String>)>>,
    said_idle: AtomicBool,
    said_missing_key: AtomicBool,
    said_too_many_skills: AtomicBool,
}

impl Advisor {
    /// An advisor over `skills` (may be empty).
    pub(crate) fn new(advisory: Advisory, skills: &crate::skills::SkillSet) -> Self {
        let mut seen = std::collections::HashSet::new();
        let skills = skills
            .skills()
            .iter()
            .filter(|s| seen.insert(s.name.clone()))
            .map(|s| {
                (
                    s.name.clone(),
                    truncate_head(&s.description, MAX_DESCRIPTION_CHARS),
                )
            })
            .collect();
        let model = advisory.model.clone().with_timeout(advisory.timeout);
        Self {
            advisory,
            model,
            skills,
            memo: Mutex::new(None),
            said_idle: AtomicBool::new(false),
            said_missing_key: AtomicBool::new(false),
            said_too_many_skills: AtomicBool::new(false),
        }
    }

    fn none_option(&self) -> &'static str {
        if self.skills.iter().any(|(n, _)| n == "none") {
            "no_skill"
        } else {
            "none"
        }
    }

    async fn advise(&self, turn: &TurnContext<'_>) -> Option<String> {
        let caps = self.model.capabilities();
        let choice_ok = caps.supports(QuestionKind::Choice);
        let too_many_skills = self.skills.len() >= caps.max_choice_options;
        if self.advisory.skill_hint
            && too_many_skills
            && !self.said_too_many_skills.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                skills = self.skills.len(),
                limit = caps.max_choice_options,
                "decision advisory skill hint off: more skills than the backend's Choice \
                 option limit (one option is reserved for \"none\")"
            );
        }
        let want_skill = self.advisory.skill_hint
            && !self.skills.is_empty()
            && choice_ok
            && caps.supports(QuestionKind::Noul)
            && !too_many_skills;
        let want_tools = self.advisory.tool_hint
            && choice_ok
            && turn.tools.len() >= self.advisory.tool_hint_min_tools.max(2)
            && turn.tools.len() <= caps.max_choice_options;
        if !want_skill && !want_tools {
            if !self.said_idle.swap(true, Ordering::Relaxed) {
                tracing::debug!(
                    skills = self.skills.len(),
                    tools = turn.tools.len(),
                    "decision advisory has nothing to ask: no skills to suggest and fewer \
                     than {} tools, so no decision request is sent",
                    self.advisory.tool_hint_min_tools
                );
            }
            return None;
        }
        let request = turn.user_request()?;

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
                Question::choice_with_criteria(
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
                Question::choice_with_criteria(
                    "Which of these tools will the assistant most likely need next to carry \
                     out `request`?",
                    turn.tools.iter().map(|t| {
                        let desc = truncate_head(&t.description, MAX_DESCRIPTION_CHARS);
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

        let state = json!({ "request": truncate_middle(&request, MAX_REQUEST_CHARS) });
        let eval = match self.model.evaluate(state, questions).await {
            Ok(eval) => eval,
            Err(DecisionError::MissingApiKey(var)) => {
                if !self.said_missing_key.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "decision advisory disabled: the decision model's API key is missing \
                         (set {var}); the agent continues without hints"
                    );
                }
                self.remember(key, None);
                return None;
            }
            Err(e) => {
                tracing::warn!("decision advisory skipped this request: {e}");
                self.remember(key, None);
                return None;
            }
        };

        let mut lines = Vec::new();
        if want_skill {
            let needed = eval.p_true("skill_needed").unwrap_or(0.0);
            if let Some(choice) = eval.choice("skill") {
                if needed >= self.advisory.skill_need_threshold
                    && choice.choice() != none
                    && choice.confidence() >= self.advisory.skill_confidence_threshold
                {
                    lines.push(format!(
                        "Relevant to the current request: {}. Ignore this if it does not fit \
                         what the user actually asked for.",
                        choice.choice()
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

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TurnHook for Advisor {
    async fn before_turn(&self, turn: &TurnContext<'_>) -> Option<String> {
        self.advise(turn).await
    }
}

/// At most `max` characters from the start, on a char boundary.
pub(crate) fn truncate_head(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// At most about `max` characters: the head and the tail, with an explicit
/// `[truncated N chars]` marker between them.
pub(crate) fn truncate_middle(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    let keep = max / 2;
    let head: String = text.chars().take(keep).collect();
    let tail: String = text.chars().skip(len - keep).collect();
    format!("{head}\n[truncated {} chars]\n{tail}", len - 2 * keep)
}
