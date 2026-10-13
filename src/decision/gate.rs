//! [`ToolGate`]: an [`Extension`](crate::Extension) that asks a decision
//! model whether a tool call is destructive and whether the user asked for
//! it. Blocking, so strictly opt-in, and fail-closed.

use super::advisory::{assert_threshold, truncate_middle};
use super::question::{check_token_limits, Question};
use super::DecisionModel;
use crate::types::{ToolCallRequest, ToolDecision, ToolMiddleware};
use serde_json::{json, Value};
use std::time::Duration;

/// Most characters of the user's request sent as state (head and tail kept).
const MAX_REQUEST_CHARS: usize = 8_000;
/// Default for [`ToolGate::with_max_argument_chars`].
const DEFAULT_MAX_ARGUMENT_CHARS: usize = 48_000;
/// The least of a long string a shortened call keeps (head and tail). A call
/// that does not fit with every shortenable string cut to this is denied.
const MIN_KEPT_CHARS: usize = 400;
/// Arguments the gate never shortens by default: a command is judged whole
/// or not at all, since what it does can sit anywhere in it.
const DEFAULT_FULL_VIEWS: &[(&str, &str)] = &[("bash", "/command")];

const DESTRUCTIVE_ID: &str = "destructive";
const REQUESTED_ID: &str = "requested";

const DEFAULT_DESTRUCTIVE_QUESTION: &str = "Would executing `tool_call` delete, overwrite, or \
     irreversibly change data or external state — files, databases, remote systems, or messages \
     sent to other people?";

const DEFAULT_REQUESTED_QUESTION: &str = "Is `tool_call` something the user asked for in \
     `user_request`, or a direct step toward what they asked for?";

/// A tool gate: per tool call, one decision request with two Nouls —
/// *destructive* ("Would executing `tool_call` delete, overwrite, or
/// irreversibly change data or external state — files, databases, remote
/// systems, or messages sent to other people?") and *requested* ("Is
/// `tool_call` something the user asked for in `user_request`, or a direct
/// step toward what they asked for?") — plus any checks you add.
///
/// The call is **denied** when it looks destructive
/// (`p_destructive >= 0.5`) **and** is not clearly requested
/// (`p_requested < 0.7`), or when any added check reaches its threshold.
/// Everything else is allowed. A denial reaches the model as an error tool
/// result with the reason, so it can ask the user instead.
///
/// The state is `{"user_request": .., "tool_call": {"tool": .., "arguments": ..}}`.
/// `user_request` is [`ToolCallRequest::user_request`]: the user's latest
/// message after the most recent compaction boundary — with, when it is a
/// short reply to an assistant question, that question and the earlier
/// request, so a confirmation ("yes, go ahead") carries what it confirms —
/// else the run's own prompts. When none of these exists (compaction left
/// no user message and the run has no prompt), the call is denied and the
/// reason asks for the request to be restated.
///
/// **Arguments are sent whole when they fit**: within
/// [`with_max_argument_chars`](Self::with_max_argument_chars) (default
/// 48,000 characters, serialized) and the decision backend's own token
/// limits ([`Capabilities`](super::Capabilities)). A call that does not fit
/// has its longest strings shortened — only as far as needed, each to its
/// head and tail around an explicit `[truncated N chars]` marker — and the
/// state then lists what was cut under `tool_call.omitted`
/// (`[{"argument": "/content", "chars": N}]`, JSON pointers), so the model
/// knows it judges a partial view. Paths, names and flags are short and
/// stay whole. Arguments registered with
/// [`with_full_view`](Self::with_full_view) — by default the built-in
/// `bash` tool's `command` — are never shortened: a call whose full-view
/// argument does not fit is denied ("more than the gate can review in full"), since a
/// dangerous line can sit anywhere in a command. A call that does not fit
/// even with every other long string cut to 400 characters is denied too.
/// Calls are never judged in pieces: what one piece does can depend on
/// another.
///
/// **Fails closed.** A decision-model error or timeout (default 5 s), a
/// missing or malformed answer, and a non-finite probability all deny.
///
/// **Scope.**
/// - Install it with [`Agent::with_tool_gate`](crate::Agent::with_tool_gate),
///   which runs it **last**, after every middleware and extension, so
///   nothing can modify the arguments after they were approved.
/// - It gates the agent it is installed on. Calls made *inside* a
///   [`SubAgentTool`](crate::SubAgentTool) are not covered by the parent's
///   gate; give the sub-agent its own (`SubAgentTool::with_tool_gate`) —
///   and there `user_request` is the task text the parent model wrote, not
///   the human's words.
///
/// **Defence in depth, not a security boundary.** The user's message and
/// the call's arguments can carry text written to steer the decision model,
/// which does not treat its state as hostile. Keep real sandboxing and
/// permissions underneath.
///
/// **This widens in the fail-open direction.** Including the assistant's
/// question and the earlier request in `user_request` makes more calls
/// count as requested — and the assistant's text can itself be steered by
/// injected content (a tool result that makes the model *ask* "Shall I
/// delete everything?" turns a user's "yes" into apparent consent).
#[derive(Debug, Clone)]
pub struct ToolGate {
    model: DecisionModel,
    timeout: Duration,
    destructive_question: String,
    requested_question: String,
    destructive_threshold: f64,
    requested_threshold: f64,
    checks: Vec<(String, String, f64)>,
    max_argument_chars: usize,
    default_full_views: bool,
    full_views: Vec<(String, String)>,
}

impl ToolGate {
    /// A gate with the default questions, thresholds and timeout.
    pub fn new(model: DecisionModel) -> Self {
        Self {
            model,
            timeout: Duration::from_secs(5),
            destructive_question: DEFAULT_DESTRUCTIVE_QUESTION.into(),
            requested_question: DEFAULT_REQUESTED_QUESTION.into(),
            destructive_threshold: 0.5,
            requested_threshold: 0.7,
            checks: Vec::new(),
            max_argument_chars: DEFAULT_MAX_ARGUMENT_CHARS,
            default_full_views: true,
            full_views: Vec::new(),
        }
    }

    /// A call counts as destructive from this probability (default 0.5).
    /// Panics outside `[0, 1]` (NaN included).
    pub fn with_destructive_threshold(mut self, p: f64) -> Self {
        assert_threshold("destructive threshold", p);
        self.destructive_threshold = p;
        self
    }

    /// A call counts as requested from this probability (default 0.7).
    /// Panics outside `[0, 1]` (NaN included).
    pub fn with_requested_threshold(mut self, p: f64) -> Self {
        assert_threshold("requested threshold", p);
        self.requested_threshold = p;
        self
    }

    /// Replace the destructive question. The state names `tool_call` and
    /// `user_request`; refer to them in backticks.
    pub fn with_destructive_question(mut self, question: impl Into<String>) -> Self {
        self.destructive_question = question.into();
        self
    }

    /// Replace the requested question.
    pub fn with_requested_question(mut self, question: impl Into<String>) -> Self {
        self.requested_question = question.into();
        self
    }

    /// Add a Noul asked on every call; the call is denied when its answer is
    /// at least `deny_at_or_above`.
    ///
    /// Panics on an empty id, an id already used (including the built-in
    /// `destructive` and `requested`), or a threshold outside `[0, 1]`.
    pub fn with_check(
        mut self,
        id: impl Into<String>,
        question: impl Into<String>,
        deny_at_or_above: f64,
    ) -> Self {
        let id = id.into();
        assert!(
            !id.trim().is_empty(),
            "a tool gate check id must not be empty"
        );
        assert!(
            id != DESTRUCTIVE_ID && id != REQUESTED_ID,
            "tool gate check id {id:?} collides with a built-in question"
        );
        assert!(
            self.checks.iter().all(|(existing, _, _)| *existing != id),
            "tool gate check id {id:?} is used twice"
        );
        assert_threshold("check threshold", deny_at_or_above);
        self.checks.push((id, question.into(), deny_at_or_above));
        self
    }

    /// Time limit per call (default 5 s); on expiry the call is denied.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Most characters of a call's arguments, serialized, sent to the
    /// decision model (default 48,000; JSON escapes count, so a newline is
    /// two). Arguments over it — or whose state would exceed the backend's
    /// token limits — are shortened as described on [`ToolGate`]. A larger budget means fewer partial views,
    /// and more input tokens (cost, latency) on large calls.
    ///
    /// Panics on 0.
    pub fn with_max_argument_chars(mut self, chars: usize) -> Self {
        assert!(chars > 0, "a tool gate's argument budget must be above 0");
        self.max_argument_chars = chars;
        self
    }

    /// Never shorten `argument` of `tool`: it is judged whole, or the call is
    /// denied. `argument` is a JSON pointer into the arguments (`"/command"`,
    /// `"/edits"` for everything under it, `""` for all of them; RFC 6901, so
    /// a key containing `/` or `~` is written with `~1` / `~0`: the key
    /// `a/b` is `"/a~1b"`). Use it
    /// for arguments that are executed — a script, a query, a program — and
    /// whose every line can matter. The built-in `bash` tool's `/command` is
    /// one by default ([`without_default_full_views`](Self::without_default_full_views)
    /// removes it).
    ///
    /// Panics on a pointer that is neither empty nor starts with `/`.
    pub fn with_full_view(mut self, tool: impl Into<String>, argument: impl Into<String>) -> Self {
        let argument = argument.into();
        assert!(
            argument.is_empty() || argument.starts_with('/'),
            "a full-view argument is a JSON pointer (\"/command\", or \"\" for all), \
             got {argument:?}"
        );
        self.full_views.push((tool.into(), argument));
        self
    }

    /// Drop the default full view (`bash`'s `/command`); ones added with
    /// [`with_full_view`](Self::with_full_view) stay, whatever the order.
    pub fn without_default_full_views(mut self) -> Self {
        self.default_full_views = false;
        self
    }

    /// The full-view pointers for `tool`.
    fn full_views_for(&self, tool: &str) -> Vec<&str> {
        let defaults = DEFAULT_FULL_VIEWS
            .iter()
            .filter(|_| self.default_full_views)
            .map(|&(t, a)| (t, a));
        let custom = self
            .full_views
            .iter()
            .map(|(t, a)| (t.as_str(), a.as_str()));
        defaults
            .chain(custom)
            .filter(|&(t, _)| t == tool)
            .map(|(_, a)| a)
            .collect()
    }

    /// Judge one call: what the gate decides when installed. For testing a
    /// gate's settings outside the loop, with a call built by
    /// [`ToolCallRequest::new`].
    pub async fn decide(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let Some(user_request) = call.user_request() else {
            tracing::warn!(
                tool = call.tool_name,
                "tool gate denied a call: no user request to judge against"
            );
            return ToolDecision::Deny(
                "Tool gate: the conversation no longer shows what the user asked for (history \
                 was compacted and this run has no prompt), so this call was not run. The gate \
                 fails closed; ask the user to restate the request."
                    .into(),
            );
        };
        let user_request = truncate_middle(&user_request, MAX_REQUEST_CHARS);

        let mut questions = vec![
            (
                DESTRUCTIVE_ID.to_string(),
                Question::noul_with_criteria(
                    self.destructive_question.as_str(),
                    "The call removes or replaces existing data, or changes something outside \
                     the conversation that cannot easily be undone.",
                    "The call only reads, lists or searches, or creates something new without \
                     replacing anything.",
                ),
            ),
            (
                REQUESTED_ID.to_string(),
                Question::noul(self.requested_question.as_str()),
            ),
        ];
        for (id, q, _) in &self.checks {
            questions.push((id.clone(), Question::noul(q.as_str())));
        }

        // The strictest limits in a fallback chain, so no member skips the call.
        let caps = self.model.chain_capabilities();
        let full_views = self.full_views_for(call.tool_name);
        let fits = |view: &View| {
            let chars =
                serde_json::to_string(&view.arguments).map_or(usize::MAX, |s| s.chars().count());
            chars <= self.max_argument_chars
                && check_token_limits(
                    &state(&user_request, call.tool_name, view),
                    &questions,
                    &caps,
                )
                .is_ok()
        };
        let view = match fit_arguments(call.args, &full_views, fits) {
            Ok(view) => view,
            Err(Unfit::FullView { argument, chars }) => {
                tracing::warn!(
                    tool = call.tool_name,
                    argument,
                    chars,
                    "tool gate denied a call: a full-view argument does not fit"
                );
                return ToolDecision::Deny(format!(
                    "Tool gate: `{}`'s `{argument}` argument is {chars} characters, more than \
                     the gate can review in full, and it does not judge part of it, so this \
                     call was not run. The gate fails closed; split it into smaller calls or \
                     ask the user.",
                    call.tool_name
                ));
            }
            Err(Unfit::TooLarge { chars }) => {
                tracing::warn!(
                    tool = call.tool_name,
                    chars,
                    "tool gate denied a call: arguments too large"
                );
                return ToolDecision::Deny(format!(
                    "Tool gate: this call's arguments are too large to evaluate: at the \
                     shortest the gate may make them ({chars} characters) they exceed its \
                     budget ({}) or the decision model's limits, so it was not run. The gate \
                     fails closed; split the work into smaller calls or ask the user.",
                    self.max_argument_chars
                ));
            }
        };
        if !view.omitted.is_empty() {
            let cut: usize = view.omitted.iter().map(|(_, chars)| chars).sum();
            let arguments: Vec<&str> = view.omitted.iter().map(|(p, _)| p.as_str()).collect();
            tracing::info!(
                tool = call.tool_name,
                ?arguments,
                chars_cut = cut,
                "tool gate judges a shortened call: it does not see the cut characters"
            );
        }
        let state = state(&user_request, call.tool_name, &view);

        let model = self.model.clone().with_timeout(self.timeout);
        let eval = match model.evaluate(state, questions).await {
            Ok(eval) => eval,
            Err(e) => {
                tracing::warn!(tool = call.tool_name, "tool gate denied a call: {e}");
                return ToolDecision::Deny(format!(
                    "Tool gate: the decision model could not be consulted ({e}), so this call \
                     was not run. The gate fails closed; ask the user how to proceed."
                ));
            }
        };

        // Every comparison below denies on a missing, non-finite or
        // out-of-range answer.
        // `evaluate` already rejects all three; this keeps the gate closed
        // even if that check ever regresses.
        let p = |id: &str| eval.p_true(id).filter(|p| (0.0..=1.0).contains(p));
        for (id, q, deny_at) in &self.checks {
            let denied = match p(id) {
                Some(v) => at_or_above(v, *deny_at),
                None => true,
            };
            if denied {
                tracing::info!(
                    tool = call.tool_name,
                    check = id.as_str(),
                    p = fmt_p(p(id)),
                    "tool gate denied a call: a check reached its threshold"
                );
                return ToolDecision::Deny(format!(
                    "Tool gate: check `{id}` (\"{q}\") answered {}, at or above {deny_at:.2}. \
                     Ask the user to confirm before retrying.",
                    fmt_p(p(id))
                ));
            }
        }
        let (destructive, requested) = (p(DESTRUCTIVE_ID), p(REQUESTED_ID));
        let looks_destructive = match destructive {
            Some(d) => at_or_above(d, self.destructive_threshold),
            None => true,
        };
        let clearly_requested = match requested {
            Some(r) => r >= self.requested_threshold,
            None => false,
        };
        if destructive.is_none() || requested.is_none() || (looks_destructive && !clearly_requested)
        {
            tracing::info!(
                tool = call.tool_name,
                destructive = fmt_p(destructive),
                requested = fmt_p(requested),
                "tool gate denied a call: destructive and not clearly requested"
            );
            return ToolDecision::Deny(format!(
                "Tool gate: this call looks destructive or irreversible (p={}) and not clearly \
                 what the user asked for (p={}). Ask the user to confirm before retrying.",
                fmt_p(destructive),
                fmt_p(requested)
            ));
        }
        ToolDecision::Allow
    }
}

/// `v >= t`, except that an unordered comparison (NaN) counts as reaching the
/// threshold — the denying side.
fn at_or_above(v: f64, t: f64) -> bool {
    !matches!(v.partial_cmp(&t), Some(std::cmp::Ordering::Less))
}

fn fmt_p(p: Option<f64>) -> String {
    p.map_or_else(|| "unknown".into(), |p| format!("{p:.2}"))
}

/// The arguments as the gate shows them, and what was cut from them.
#[derive(Debug)]
struct View {
    arguments: Value,
    /// `(JSON pointer, characters cut)` per shortened string.
    omitted: Vec<(String, usize)>,
}

#[derive(Debug, PartialEq)]
enum Unfit {
    /// A full-view argument is what does not fit.
    FullView { argument: String, chars: usize },
    /// Nothing more can be cut.
    TooLarge { chars: usize },
}

/// The state the decision model sees.
fn state(user_request: &str, tool: &str, view: &View) -> Value {
    let mut call = json!({ "tool": tool, "arguments": view.arguments });
    if !view.omitted.is_empty() {
        call["omitted"] = view
            .omitted
            .iter()
            .map(|(argument, chars)| json!({ "argument": argument, "chars": chars }))
            .collect();
    }
    json!({ "user_request": user_request, "tool_call": call })
}

/// The largest view of `args` that `fits`: whole when it fits; else every
/// string outside `full_views` longer than some cap `c` cut to `c` (head and
/// tail), with `c` as large as fits (at least `MIN_KEPT_CHARS`).
fn fit_arguments(
    args: &Value,
    full_views: &[&str],
    fits: impl Fn(&View) -> bool,
) -> Result<View, Unfit> {
    let whole = View {
        arguments: args.clone(),
        omitted: Vec::new(),
    };
    if fits(&whole) {
        return Ok(whole);
    }
    // At a cap, two views: only the cuts that shrink the whole state, the
    // `omitted` entries included (what the token limits measure), else every
    // cut that shrinks the arguments (what the character budget measures). A
    // marker and entry can outweigh a small cut, so neither rule is right for
    // both limits; the first that fits wins, the first keeping more text. A
    // call that fits only with some of the second view's extra cuts, both
    // limits binding at once, is still denied: no search over subsets.
    let at = |cap: usize, full_views: &[&str]| -> Option<View> {
        let lean = cut_by(args, full_views, cap, Rule::State);
        if fits(&lean) {
            return Some(lean);
        }
        let all = cut_by(args, full_views, cap, Rule::Arguments);
        (all.omitted.len() != lean.omitted.len() && fits(&all)).then_some(all)
    };
    let longest = longest_string(args, &mut String::new(), full_views, false);
    if longest > MIN_KEPT_CHARS {
        if let Some(mut best) = at(MIN_KEPT_CHARS, full_views) {
            // Binary search the largest cap that fits: `lo` fits, `hi` does not.
            let (mut lo, mut hi) = (MIN_KEPT_CHARS, longest);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                match at(mid, full_views) {
                    Some(view) => {
                        best = view;
                        lo = mid;
                    }
                    None => hi = mid,
                }
            }
            return Ok(best);
        }
    }
    // It does not fit even cut to the minimum. Blame a full-view argument when
    // cutting it too would have made it fit.
    let mut views = Vec::new();
    collect_full_views(args, &mut String::new(), full_views, false, &mut views);
    if let Some((argument, chars)) = views.into_iter().max_by_key(|&(_, chars)| chars) {
        if chars > MIN_KEPT_CHARS && at(MIN_KEPT_CHARS, &[]).is_some() {
            return Err(Unfit::FullView { argument, chars });
        }
    }
    // The smallest view the gate may send, for the denial to report.
    let chars = json_chars(&cut(args, full_views, MIN_KEPT_CHARS).arguments);
    Err(Unfit::TooLarge { chars })
}

fn is_full_view(pointer: &str, full_views: &[&str]) -> bool {
    full_views.iter().any(|v| {
        v.is_empty()
            || pointer == *v
            || (pointer.starts_with(*v) && pointer.as_bytes().get(v.len()) == Some(&b'/'))
    })
}

/// `pointer` extended by one JSON-pointer segment (RFC 6901 escaping).
fn push_segment(pointer: &mut String, segment: &str) -> usize {
    let len = pointer.len();
    pointer.push('/');
    pointer.push_str(&segment.replace('~', "~0").replace('/', "~1"));
    len
}

/// Visit every string in `v` with its pointer and whether it is full-view.
fn for_each_string(
    v: &Value,
    pointer: &mut String,
    full_views: &[&str],
    inherited: bool,
    f: &mut impl FnMut(&str, &str, bool),
) {
    let here = inherited || is_full_view(pointer, full_views);
    match v {
        Value::String(s) => f(pointer, s, here),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let len = push_segment(pointer, &i.to_string());
                for_each_string(item, pointer, full_views, here, f);
                pointer.truncate(len);
            }
        }
        Value::Object(map) => {
            for (k, item) in map {
                let len = push_segment(pointer, k);
                for_each_string(item, pointer, full_views, here, f);
                pointer.truncate(len);
            }
        }
        _ => {}
    }
}

/// The longest string outside the full views, in characters.
fn longest_string(v: &Value, pointer: &mut String, full_views: &[&str], inherited: bool) -> usize {
    let mut longest = 0;
    for_each_string(v, pointer, full_views, inherited, &mut |_, s, full| {
        if !full {
            longest = longest.max(s.chars().count());
        }
    });
    longest
}

/// Every string inside a full view, with its pointer and length.
fn collect_full_views(
    v: &Value,
    pointer: &mut String,
    full_views: &[&str],
    inherited: bool,
    out: &mut Vec<(String, usize)>,
) {
    for_each_string(v, pointer, full_views, inherited, &mut |p, s, full| {
        if full {
            out.push((p.to_string(), s.chars().count()));
        }
    });
}

/// Characters `v` takes serialized.
fn json_chars(v: &impl serde::Serialize) -> usize {
    json_size(v).0
}

/// Characters and bytes `v` takes serialized.
fn json_size(v: &impl serde::Serialize) -> (usize, usize) {
    serde_json::to_string(v).map_or((usize::MAX, usize::MAX), |s| (s.chars().count(), s.len()))
}

/// `args` with every string outside `full_views` longer than `cap` cut to
/// its head and tail — except where the cut, with its marker, would not
/// make the arguments smaller.
fn cut(args: &Value, full_views: &[&str], cap: usize) -> View {
    cut_by(args, full_views, cap, Rule::Arguments)
}

/// When a cut counts as saving space.
#[derive(Clone, Copy)]
enum Rule {
    /// It shrinks the serialized arguments (the character budget).
    Arguments,
    /// It shrinks the whole state, its `omitted` entry included (the token
    /// limits), in characters and in bytes.
    State,
}

/// What a first `omitted` entry adds to the state besides itself.
const OMITTED_KEY: &str = r#","omitted":[]"#;

/// [`cut`] under `rule`.
fn cut_by(args: &Value, full_views: &[&str], cap: usize, rule: Rule) -> View {
    fn go(
        v: &Value,
        pointer: &mut String,
        full_views: &[&str],
        inherited: bool,
        cap: usize,
        rule: Rule,
        omitted: &mut Vec<(String, usize)>,
    ) -> Value {
        let here = inherited || is_full_view(pointer, full_views);
        match v {
            Value::String(s) if !here => {
                let len = s.chars().count();
                if len <= cap {
                    return v.clone();
                }
                // Only a cut that saves space under `rule`: the marker (and
                // the `omitted` entry) can outweigh the few characters cut
                // from a string just over `cap`. Under `Arguments`, a cut that
                // saves characters saves bytes too (every removed character
                // is at least a byte, the marker is ASCII); an entry's pointer
                // need not be ASCII, so `State` checks both.
                let chars = len - 2 * (cap / 2);
                let shortened = truncate_middle(s, cap);
                let (before, after) = (json_size(s), json_size(&shortened));
                let extra = match rule {
                    Rule::Arguments => (0, 0),
                    Rule::State => {
                        let entry =
                            json_size(&json!({ "argument": pointer.as_str(), "chars": chars }));
                        let opening = if omitted.is_empty() {
                            OMITTED_KEY.len()
                        } else {
                            1
                        };
                        (entry.0 + opening, entry.1 + opening)
                    }
                };
                if after.0.saturating_add(extra.0) >= before.0
                    || after.1.saturating_add(extra.1) > before.1
                {
                    return v.clone();
                }
                omitted.push((pointer.clone(), chars));
                Value::String(shortened)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let len = push_segment(pointer, &i.to_string());
                        let out = go(item, pointer, full_views, here, cap, rule, omitted);
                        pointer.truncate(len);
                        out
                    })
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, item)| {
                        let len = push_segment(pointer, k);
                        let out = go(item, pointer, full_views, here, cap, rule, omitted);
                        pointer.truncate(len);
                        (k.clone(), out)
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let mut omitted = Vec::new();
    let arguments = go(
        args,
        &mut String::new(),
        full_views,
        false,
        cap,
        rule,
        &mut omitted,
    );
    View { arguments, omitted }
}

/// **Deprecated since 0.25**, to be removed in a later release: install the
/// gate with [`Agent::with_tool_gate`](crate::Agent::with_tool_gate) (or as an
/// extension, `with_extension` / `with_tree_extension`), and call
/// [`ToolGate::decide`] to drive it outside the loop. Installed as a
/// middleware it runs before every extension and before any middleware added
/// after it, so arguments either rewrites are never judged. Rust cannot mark
/// a trait impl `#[deprecated]`, so the first use in the process logs a
/// warning instead.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl ToolMiddleware for ToolGate {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        if super::warn_once("deprecated:ToolGate as ToolMiddleware".into()) {
            tracing::warn!(
                "ToolGate used as a ToolMiddleware is deprecated: install it with \
                 with_tool_gate (or as an extension), or call ToolGate::decide"
            );
        }
        self.decide(call).await
    }
}

/// The gate as an [`Extension`](crate::Extension): what
/// [`Agent::with_tool_gate`](crate::Agent::with_tool_gate) installs. Its
/// `before_tool` is [`ToolGate::decide`].
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl crate::Extension for ToolGate {
    fn name(&self) -> &str {
        "tool-gate"
    }

    /// A gate judges the arguments that will run, also when it is installed
    /// ahead of an extension that rewrites them (as a tree extension is).
    fn rechecks_modified_calls(&self) -> bool {
        true
    }

    async fn start_run(
        &self,
        _run: &crate::extension::RunContext<'_>,
    ) -> Result<Box<dyn crate::RunHooks>, crate::extension::ExtensionError> {
        Ok(Box::new(GateHooks(self.clone())))
    }
}

struct GateHooks(ToolGate);

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl crate::RunHooks for GateHooks {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.0.decide(call).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_views_match_whole_segments_only() {
        assert!(is_full_view("/command", &["/command"]));
        assert!(is_full_view("/edits/0/new", &["/edits"]));
        assert!(!is_full_view("/commands", &["/command"]));
        assert!(!is_full_view("/command", &["/commands"]));
        assert!(is_full_view("/anything", &[""]));
        assert!(!is_full_view("/command", &[]));
    }

    #[test]
    fn pointers_escape_tilde_and_slash() {
        let args = json!({"a/b": "x".repeat(1_000), "c~d": "y".repeat(1_000)});
        let view = cut(&args, &[], MIN_KEPT_CHARS);
        let mut cut: Vec<_> = view.omitted.iter().map(|(p, _)| p.as_str()).collect();
        cut.sort_unstable();
        assert_eq!(cut, ["/a~1b", "/c~0d"]);
        // An escaped full view protects its key.
        let view = super::cut(&args, &["/a~1b"], MIN_KEPT_CHARS);
        assert_eq!(view.omitted.len(), 1);
        assert_eq!(view.arguments["a/b"], args["a/b"]);
    }

    #[test]
    fn fit_cuts_the_longest_strings_first() {
        // Budget 3,000: the short string stays whole; the long one takes the cut.
        let args = json!({"short": "s".repeat(900), "long": "l".repeat(10_000)});
        let fits = |v: &View| serde_json::to_string(&v.arguments).unwrap().len() <= 3_000;
        let view = fit_arguments(&args, &[], fits).unwrap();
        assert_eq!(view.arguments["short"], args["short"]);
        assert_eq!(view.omitted.len(), 1);
        assert_eq!(view.omitted[0].0, "/long");
        let len = serde_json::to_string(&view.arguments).unwrap().len();
        assert!((2_900..=3_000).contains(&len), "{len}");
    }

    #[test]
    fn a_cut_never_grows_the_arguments() {
        // Strings around the cap (where the marker outweighs the cut),
        // escapes that shrink when cut, and non-ASCII text.
        let args = json!({
            "near": "n".repeat(401),
            "escaped": "\n".repeat(600),
            "cjk": "汉".repeat(450),
            "long": "l".repeat(5_000),
            "nested": [{"deep": "d".repeat(430)}],
        });
        let whole = serde_json::to_string(&args).unwrap();
        let (chars, bytes) = (whole.chars().count(), whole.len());
        for cap in [MIN_KEPT_CHARS, 401, 420, 450, 1_000, 4_999] {
            let view = cut(&args, &[], cap);
            let s = serde_json::to_string(&view.arguments).unwrap();
            assert!(s.chars().count() <= chars, "cap {cap}: chars grew");
            assert!(s.len() <= bytes, "cap {cap}: bytes grew");
            // Every listed cut really happened, and only those.
            for (pointer, _) in &view.omitted {
                let shown = view.arguments.pointer(pointer).unwrap().as_str().unwrap();
                assert!(shown.contains("[truncated "), "{pointer}");
            }
        }
        // Just over the cap: left whole, not listed.
        let view = cut(&args, &[], 400);
        assert_eq!(view.arguments["near"], args["near"]);
        assert!(view.omitted.iter().all(|(p, _)| p != "/near"));
    }

    #[test]
    fn a_state_rule_cut_never_grows_the_state() {
        // Long and non-ASCII keys make the `omitted` entry outweigh a cut
        // that shortens the arguments: under `Rule::State` it is not made.
        let args = json!({
            "near": "n".repeat(450),
            "键_long_non_ascii_key_键键键键键键": "k".repeat(470),
            "long": "l".repeat(5_000),
            "cjk": "汉".repeat(600),
        });
        let whole = View {
            arguments: args.clone(),
            omitted: Vec::new(),
        };
        let size = |v: &View| json_size(&state("go", "t", v));
        let (chars, bytes) = size(&whole);
        for cap in [MIN_KEPT_CHARS, 420, 460, 1_000, 4_999] {
            let view = cut_by(&args, &[], cap, Rule::State);
            let (c, b) = size(&view);
            assert!(
                c <= chars && b <= bytes,
                "cap {cap}: ({c}, {b}) > ({chars}, {bytes})"
            );
        }
        // The non-ASCII key's cut shortens the arguments but not the state.
        let lean = cut_by(&args, &[], MIN_KEPT_CHARS, Rule::State);
        let all = cut_by(&args, &[], MIN_KEPT_CHARS, Rule::Arguments);
        let key = "/键_long_non_ascii_key_键键键键键键";
        assert!(all.omitted.iter().any(|(p, _)| p == key));
        assert!(lean.omitted.iter().all(|(p, _)| p != key));
    }

    #[test]
    fn fit_blames_the_full_view_that_does_not_fit() {
        let args = json!({"command": "c".repeat(10_000), "note": "n".repeat(10_000)});
        let fits = |v: &View| serde_json::to_string(&v.arguments).unwrap().len() <= 3_000;
        assert_eq!(
            fit_arguments(&args, &["/command"], fits).unwrap_err(),
            Unfit::FullView {
                argument: "/command".into(),
                chars: 10_000
            }
        );
        // With room for nothing at all, it is simply too large.
        let none = |_: &View| false;
        assert!(matches!(
            fit_arguments(&args, &["/command"], none),
            Err(Unfit::TooLarge { .. })
        ));
    }
}
