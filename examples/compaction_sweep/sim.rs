//! Drives the real compaction code the way `agent_loop.rs` does, and measures.
//!
//! Per request, in loop order:
//!
//! 1. measure growth since the last compaction step (the loop's rolling mean);
//! 2. resolve the ratio with the real [`ContextConfig::effective_target_ratio`];
//! 3. compact with the real [`compact_messages`] — or the real
//!    [`LlmCompaction`], fed its result back;
//! 4. account for the request that would be sent;
//! 5. append the response, capping tool output on the way in with the real
//!    [`truncate_tool_output_keyed`], as `truncate_tool_output_on_append` does.
//!
//! What is *not* the loop: the budget is the calibrated one, held fixed
//! (`system_prompt_tokens: 0`), where the loop re-derives it from provider
//! usage every turn. Every figure is therefore "at an effective message budget
//! of N tokens".

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use yoagent::context::{
    compact_messages, message_tokens, total_tokens, truncate_tool_output_keyed, CompactionStrategy,
    ContextConfig,
};
use yoagent::provider::{ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider};
use yoagent::types::{
    AgentEvent, AgentMessage, CompactionMethod, Content, Message, StopReason, Usage,
};
use yoagent::LlmCompaction;

use crate::profiles::{task_prompt, Generator, Profile, TASK_PREFIX, USER_PREFIX};

/// One configuration under test.
#[derive(Clone)]
pub struct Knobs {
    pub budget: usize,
    pub keep_first: usize,
    pub keep_recent: usize,
    pub target_ratio: f32,
    pub headroom: Option<usize>,
    pub max_lines: usize,
    /// Candidate `MIN_HEADROOM_RATIO`, applied as `real_ratio.max(floor)`.
    ///
    /// Exact for any floor >= the shipped one: the real function returns
    /// `max(min(derived, ratio), MIN)`, and `max(max(x, MIN), f) == max(x, f)`
    /// whenever `f >= MIN`. Only honoured when the headroom policy is on,
    /// because that is the only branch the real floor lives in.
    pub floor: Option<f32>,
    /// Candidate policy, not shipped: never let the ratio fall below what the
    /// protected set (`keep_first` head + marker + `keep_recent` tail) needs.
    pub protect: bool,
    pub llm: Option<LlmKnobs>,
    /// Print one line per compaction, in the shape of `long_horizon`'s log.
    pub trace: bool,
}

#[derive(Clone, Copy)]
pub struct LlmKnobs {
    pub trigger: f32,
    /// Loop turns a summarization request takes to come back.
    pub latency: usize,
}

impl Knobs {
    /// Every knob at the shipped default, read from `ContextConfig::default()`
    /// so a changed default changes the baseline rather than silently
    /// diverging from it.
    pub fn defaults(budget: usize) -> Self {
        let d = ContextConfig::default();
        Self {
            budget,
            keep_first: d.keep_first,
            keep_recent: d.keep_recent,
            target_ratio: d.compact_target_ratio,
            headroom: d.compact_headroom_turns,
            max_lines: d.tool_output_max_lines,
            floor: None,
            protect: false,
            llm: None,
            trace: false,
        }
    }

    fn config(&self) -> ContextConfig {
        ContextConfig {
            max_context_tokens: self.budget,
            system_prompt_tokens: 0,
            keep_first: self.keep_first,
            keep_recent: self.keep_recent,
            tool_output_max_lines: self.max_lines,
            compact_target_ratio: self.target_ratio,
            compact_headroom_turns: self.headroom,
            ..Default::default()
        }
    }
}

/// Everything measured over one or more sessions. Sums, so seeds aggregate by
/// addition; ratios are derived at print time.
#[derive(Default, Clone)]
pub struct Metrics {
    pub requests: usize,
    /// Compaction steps that changed history (the loop's own test).
    pub compactions: usize,
    /// Input tokens sent, summed over requests.
    pub input: usize,
    /// Of those, the prefix shared verbatim with the previous request — what
    /// an ideal prefix cache would serve.
    pub cached: usize,
    pub after_sum: usize,
    pub after_min: usize,
    /// Distinct turns whose assistant message or tool output is still present
    /// verbatim (not summarized, not dropped), summed over requests.
    pub detail_turns: usize,
    /// Compactions after which the opening task prompt is gone.
    pub head_lost: usize,
    /// Compactions after which the most recent user message is gone.
    pub ask_lost: usize,
    /// Compactions that left nothing but the compaction marker.
    pub marker_only: usize,
    /// Dangling tool calls or tool results in any request (must stay 0).
    pub orphans: usize,
    /// Tool results appended, and how many the append-path cap truncated.
    pub tool_results: usize,
    pub truncated: usize,
    pub tool_tokens_raw: usize,
    pub tool_tokens_kept: usize,
    /// Tokens appended per request (after the cap), summed.
    pub appended: usize,
    // LlmCompaction only.
    pub llm_requests: usize,
    pub splices: usize,
    pub llm_compactions: usize,
    /// Turns between a summarization request starting and the next budget
    /// crossing — the window the briefing has to land in.
    pub windows: Vec<usize>,
}

impl Metrics {
    pub fn add(&mut self, o: &Metrics) {
        let min = if self.requests == 0 {
            o.after_min
        } else {
            self.after_min.min(o.after_min)
        };
        self.requests += o.requests;
        self.compactions += o.compactions;
        self.input += o.input;
        self.cached += o.cached;
        self.after_sum += o.after_sum;
        self.after_min = min;
        self.detail_turns += o.detail_turns;
        self.head_lost += o.head_lost;
        self.ask_lost += o.ask_lost;
        self.marker_only += o.marker_only;
        self.orphans += o.orphans;
        self.tool_results += o.tool_results;
        self.truncated += o.truncated;
        self.tool_tokens_raw += o.tool_tokens_raw;
        self.tool_tokens_kept += o.tool_tokens_kept;
        self.appended += o.appended;
        self.llm_requests += o.llm_requests;
        self.splices += o.splices;
        self.llm_compactions += o.llm_compactions;
        self.windows.extend_from_slice(&o.windows);
    }
}

fn texts(m: &AgentMessage) -> impl Iterator<Item = &str> {
    let content: &[Content] = match m {
        AgentMessage::Llm(Message::User { content, .. })
        | AgentMessage::Llm(Message::Assistant { content, .. })
        | AgentMessage::Llm(Message::ToolResult { content, .. }) => content,
        AgentMessage::Custom(_) => &[],
    };
    content.iter().filter_map(|c| match c {
        Content::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

fn is_user_text(m: &AgentMessage, prefix: &str, ts: Option<u64>) -> bool {
    match m {
        AgentMessage::Llm(Message::User { timestamp, .. }) => {
            ts.is_none_or(|t| *timestamp == t) && texts(m).any(|t| t.starts_with(prefix))
        }
        _ => false,
    }
}

fn orphans(messages: &[AgentMessage]) -> usize {
    let mut calls = HashSet::new();
    let mut results = HashSet::new();
    for m in messages {
        match m {
            AgentMessage::Llm(Message::Assistant { content, .. }) => {
                for c in content {
                    if let Content::ToolCall { id, .. } = c {
                        calls.insert(id.clone());
                    }
                }
            }
            AgentMessage::Llm(Message::ToolResult { tool_call_id, .. }) => {
                results.insert(tool_call_id.clone());
            }
            _ => {}
        }
    }
    calls.symmetric_difference(&results).count()
}

fn detail_turns(messages: &[AgentMessage]) -> usize {
    let mut turns = HashSet::new();
    for m in messages {
        match m {
            AgentMessage::Llm(Message::Assistant { timestamp, .. })
            | AgentMessage::Llm(Message::ToolResult { timestamp, .. }) => {
                turns.insert(*timestamp);
            }
            _ => {}
        }
    }
    turns.len()
}

fn common_prefix_tokens(prev: &[AgentMessage], next: &[AgentMessage]) -> usize {
    prev.iter()
        .zip(next)
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| message_tokens(a))
        .sum()
}

/// The candidate "protect" floor: head + marker + tail as a share of budget.
/// An approximation of what level 3 must keep; used only to pick a ratio.
fn protected_ratio(messages: &[AgentMessage], cfg: &ContextConfig) -> f32 {
    let n = messages.len();
    let head: usize = messages[..cfg.keep_first.min(n)]
        .iter()
        .map(message_tokens)
        .sum();
    let tail: usize = messages[n.saturating_sub(cfg.keep_recent)..]
        .iter()
        .map(message_tokens)
        .sum();
    let marker = 22; // `COMPACTION_MARKER` as a message, by `message_tokens`
                     // +1%: `compaction_target` truncates `budget * ratio`, and an f32 ratio
                     // that lands one token under the protected set sends level 3 straight to
                     // `keep_within_budget` — the very path this candidate exists to avoid.
    ((((head + tail + marker) as f64 / cfg.max_context_tokens as f64) as f32) + 0.01).min(0.95)
}

// ---------------------------------------------------------------------------
// LlmCompaction harness: a summarizer whose latency is counted in loop turns
// ---------------------------------------------------------------------------

struct Gate {
    turn: AtomicUsize,
    /// Requests that have come back — with `spawns`, the progress signal
    /// `settle` waits on.
    done: AtomicUsize,
    pending: Mutex<Vec<(usize, tokio::sync::oneshot::Sender<()>)>>,
    spawns: Mutex<Vec<usize>>,
}

/// Blocks each summarization request until the simulation releases it, so
/// "how long the summarizer takes" is a whole number of loop turns rather
/// than wall-clock — which is what makes the splice race deterministic.
struct GatedProvider(Arc<Gate>);

#[async_trait::async_trait]
impl StreamProvider for GatedProvider {
    async fn stream(
        &self,
        _config: StreamConfig,
        _tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let now = self.0.turn.load(Ordering::SeqCst);
        self.0.spawns.lock().unwrap().push(now);
        self.0.pending.lock().unwrap().push((now, tx));
        let _ = rx.await;
        self.0.done.fetch_add(1, Ordering::SeqCst);
        // A ~800-token briefing, the order of what a real one runs to.
        Ok(Message::assistant(
            vec![Content::Text {
                text: format!("## Goal\n{}\n## Open items\nnone", "b".repeat(3_200)),
            }],
            StopReason::Stop,
            "sim",
            "sim",
            Usage::default(),
        ))
    }
}

/// Let spawned summarization tasks run until nothing moves.
///
/// Single-threaded runtime, no I/O: a spawned request needs one poll to
/// register with the gate, and a released one needs one poll to return and
/// publish its briefing (`Phase::Ready` is set in the same poll the provider
/// returns in). Yield until neither counter has changed for three consecutive
/// yields — deterministic, and far cheaper than a fixed large count.
async fn settle(gate: &Gate) {
    let progress = || {
        (
            gate.spawns.lock().unwrap().len(),
            gate.done.load(Ordering::SeqCst),
        )
    };
    let mut last = progress();
    let mut still = 0;
    while still < 3 {
        tokio::task::yield_now().await;
        let now = progress();
        if now == last {
            still += 1;
        } else {
            (last, still) = (now, 0);
        }
    }
}

struct LlmRun {
    gate: Arc<Gate>,
    strategy: LlmCompaction,
    events: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    latency: usize,
}

impl LlmRun {
    fn new(k: LlmKnobs) -> Self {
        let gate = Arc::new(Gate {
            turn: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            pending: Mutex::new(Vec::new()),
            spawns: Mutex::new(Vec::new()),
        });
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let strategy = LlmCompaction::from_provider(
            Arc::new(GatedProvider(gate.clone())),
            ModelConfig::mock(),
        )
        .with_trigger_ratio(k.trigger)
        .with_event_sender(tx);
        Self {
            gate,
            strategy,
            events,
            latency: k.latency,
        }
    }

    async fn release_due(&self, turn: usize) {
        self.gate.turn.store(turn, Ordering::SeqCst);
        let due: Vec<_> = {
            let mut pending = self.gate.pending.lock().unwrap();
            let (due, keep) = std::mem::take(&mut *pending)
                .into_iter()
                .partition(|(s, _)| turn - s >= self.latency);
            *pending = keep;
            due
        };
        for (_, tx) in due {
            let _ = tx.send(());
        }
        settle(&self.gate).await;
    }
}

/// Drive one session of `requests` requests and measure it.
pub async fn simulate(profile: Profile, seed: u64, requests: usize, k: &Knobs) -> Metrics {
    let config = k.config();
    let mut gen = Generator::new(profile, seed);
    let mut llm = k.llm.map(LlmRun::new);
    let mut m = Metrics {
        after_min: usize::MAX,
        ..Default::default()
    };

    let mut messages = vec![task_prompt()];
    let mut prev: Vec<AgentMessage> = Vec::new();
    let mut last_after: Option<usize> = None;
    let (mut growth_total, mut growth_samples) = (0usize, 0usize);
    let (mut latest_ask_prefix, mut latest_ask_ts) = (TASK_PREFIX, 0u64);
    let mut crossings: Vec<usize> = Vec::new();

    for r in 1..=requests {
        if let Some(run) = &llm {
            run.release_due(r).await;
        }

        // 1–2. Growth and ratio, as `run_loop` computes them.
        let before_tokens = total_tokens(&messages);
        if let Some(prev_after) = last_after {
            growth_samples += 1;
            growth_total += before_tokens.saturating_sub(prev_after);
        }
        let growth = if growth_samples > 0 {
            growth_total as f64 / growth_samples as f64
        } else {
            0.0
        };
        let mut ratio = config.effective_target_ratio(growth);
        if let (Some(f), Some(_)) = (k.floor, config.compact_headroom_turns) {
            ratio = ratio.max(f);
        }
        if k.protect {
            ratio = ratio.max(protected_ratio(&messages, &config));
        }
        let effective = ContextConfig {
            compact_target_ratio: ratio,
            ..config.clone()
        };

        // 3. Compact.
        if before_tokens > k.budget {
            crossings.push(r);
        }
        let before_len = messages.len();
        messages = match &llm {
            Some(run) => run
                .strategy
                .compact(std::mem::take(&mut messages), &effective),
            None => compact_messages(std::mem::take(&mut messages), &effective),
        };
        let after_tokens = total_tokens(&messages);
        if messages.len() != before_len || after_tokens != before_tokens {
            if k.trace {
                println!(
                    "    request {r:>3}: ratio {ratio:.3}  {before_len:>3} msgs / {before_tokens:>6} tok \
                     -> {:>3} msgs / {after_tokens:>6} tok   task prompt kept: {}",
                    messages.len(),
                    messages.iter().any(|x| is_user_text(x, TASK_PREFIX, None)),
                );
            }
            m.compactions += 1;
            m.after_sum += after_tokens;
            m.after_min = m.after_min.min(after_tokens);
            if !messages.iter().any(|x| is_user_text(x, TASK_PREFIX, None)) {
                m.head_lost += 1;
            }
            if !messages
                .iter()
                .any(|x| is_user_text(x, latest_ask_prefix, Some(latest_ask_ts)))
            {
                m.ask_lost += 1;
            }
            if messages.len() == 1 && !is_user_text(&messages[0], TASK_PREFIX, None) {
                m.marker_only += 1;
            }
        }
        last_after = Some(after_tokens);
        if let Some(run) = &mut llm {
            settle(&run.gate).await;
            while let Ok(ev) = run.events.try_recv() {
                if let AgentEvent::ContextCompacted { method, .. } = ev {
                    m.llm_compactions += 1;
                    if method == CompactionMethod::Summarized {
                        m.splices += 1;
                    }
                }
            }
        }

        // 4. The request.
        m.requests += 1;
        m.input += after_tokens;
        m.cached += common_prefix_tokens(&prev, &messages);
        m.detail_turns += detail_turns(&messages);
        m.orphans += orphans(&messages);
        prev = messages.clone();

        // 5. The response, capped on append.
        let step = gen.step(r);
        let before_append = total_tokens(&messages);
        messages.push(step.assistant);
        for res in step.results {
            let raw = message_tokens(&res);
            let (capped, marked) = truncate_tool_output_keyed(res, &config, None);
            let kept = message_tokens(&capped);
            m.tool_results += 1;
            m.tool_tokens_raw += raw;
            m.tool_tokens_kept += kept;
            if !marked.is_empty() || kept < raw {
                m.truncated += 1;
            }
            messages.push(capped);
        }
        if let Some(u) = step.user {
            latest_ask_prefix = USER_PREFIX;
            latest_ask_ts = r as u64;
            messages.push(u);
        }
        m.appended += total_tokens(&messages) - before_append;
    }

    if m.after_min == usize::MAX {
        m.after_min = 0;
    }
    if let Some(run) = &llm {
        let spawns = run.gate.spawns.lock().unwrap().clone();
        m.llm_requests = spawns.len();
        for s in spawns {
            if let Some(c) = crossings.iter().find(|&&c| c > s) {
                m.windows.push(c - s);
            }
        }
    }
    m
}
