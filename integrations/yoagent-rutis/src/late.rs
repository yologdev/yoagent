//! Restricted handlers registered while a run is going: their tools offered
//! from the run's next turn ([`LateTools`]).
//!
//! A run works with the handlers registered when it started (see the
//! [`registry`](crate::registry) docs). A **restricted** handler (tools
//! only) that registers while one of its extension's own runs is going joins
//! that run: [`LateTools`], a yoagent `ToolSource` paired with the
//! extension, offers its tools from the run's next turn (yoagent asks a
//! source again when its generation moves), and the run calls its
//! `before_tool` for its own tools and its `finish` at the end. Handlers
//! with other hooks still wait for the next run: a policy must not start
//! judging in the middle of one.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use yoagent::{AgentTool, ToolSource};

use crate::extension::{call, Missed, Timeouts};
use crate::handler::{Offer, RunInfo};
use crate::registry::{Judge, LiveTool, Registered, ToolOwner};

/// The host's naming of a restricted handler's tools: given the handler's
/// name and the tool's own, the name to offer it under, or `None` to leave
/// it out.
pub(crate) type Names = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// The name `own` (a tool of `h`) is offered under: a restricted handler's
/// as the host names it ([`RutisExtension::with_restricted_tools`](crate::RutisExtension::with_restricted_tools)),
/// else `<handler>__<tool>` — kept apart from the names the host and trusted
/// plugins use; `None` when the host leaves it out. Other handlers' tools
/// keep their own names.
pub(crate) fn offered_name(names: &Option<Names>, h: &Registered, own: &str) -> Option<String> {
    if !h.restricted {
        return Some(own.to_string());
    }
    match names {
        Some(names) => names(&h.name, own),
        None => Some(format!("{}__{own}", h.name)),
    }
}

/// `tool` of `h`, offered as `name` in `run`: its owner record, and the tool
/// — for a restricted handler with `before_tool`, one that has it judge its
/// own calls when they run.
pub(crate) fn live(
    h: &Registered,
    tool: Arc<dyn AgentTool>,
    source: Option<String>,
    name: String,
    run: &RunInfo,
    policy: Option<std::time::Duration>,
) -> (ToolOwner, Arc<dyn AgentTool>) {
    let own = tool.name().to_string();
    let owner = ToolOwner {
        handler: h.name.to_string(),
        owner: h.owner.to_string(),
        source,
        plugin_name: (name != own).then(|| own.clone()),
    };
    let judge = (h.restricted && h.hooks.before_tool).then(|| Judge {
        run: run.clone(),
        owner: owner.clone(),
        timeout: policy,
    });
    let tool = Arc::new(LiveTool {
        inner: tool,
        offered_as: (name != own).then_some(name),
        judge,
        gate: h.gate.clone(),
        handler: h.clone(),
    });
    (owner, tool)
}

/// An extension's state shared with its [`LateTools`]: the extension's own
/// runs in progress.
#[derive(Default)]
pub(crate) struct Late {
    /// `late_tools` was called: the registry tells us of restricted
    /// handlers, and runs are tracked.
    pub(crate) watched: AtomicBool,
    runs: Mutex<Vec<Arc<LateRun>>>,
    generation: AtomicU64,
    pub(crate) names: Mutex<Option<Names>>,
    /// Several own runs at once was logged (once).
    crowded: AtomicBool,
}

/// One of the extension's own runs, as late handlers join it.
pub(crate) struct LateRun {
    pub(crate) run: RunInfo,
    timeouts: Timeouts,
    names: Option<Names>,
    /// Handlers the run started with: they are not late.
    snapshot: Mutex<HashSet<Arc<str>>>,
    /// Registered since, not yet asked for their tools.
    pending: Mutex<Vec<Registered>>,
    /// Asked: called for their own tools' `before_tool`, and `finish`.
    joined: Mutex<Vec<Registered>>,
    tools: Mutex<Vec<Arc<dyn AgentTool>>>,
    /// Which late handler offered each late tool.
    offered: Mutex<HashMap<String, ToolOwner>>,
    /// The names the run already offers — the extension's tools, and every
    /// tool its requests offered (the agent's, its sources', every
    /// extension's): a late tool may not take one.
    pub(crate) taken: Mutex<HashSet<String>>,
}

impl LateRun {
    /// The handlers the run started with (taken just after it was tracked):
    /// one that registered meanwhile is in both, and is not late.
    pub(crate) fn snapshot(&self, handlers: &[Registered]) {
        let names: HashSet<Arc<str>> = handlers.iter().map(|h| h.name.clone()).collect();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|h| !names.contains(&h.name));
        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = names;
    }

    pub(crate) fn joined(&self) -> Vec<Registered> {
        self.joined
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn offered(&self, tool: &str) -> Option<ToolOwner> {
        self.offered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(tool)
            .cloned()
    }
}

impl Late {
    /// A run of the extension started: tracked when it is the extension's
    /// own (not inherited as a tree extension) and late tools are in use.
    pub(crate) fn started(
        &self,
        run: &RunInfo,
        inherited: bool,
        timeouts: Timeouts,
    ) -> Option<Arc<LateRun>> {
        if inherited || !self.watched.load(Ordering::SeqCst) {
            return None;
        }
        let late = Arc::new(LateRun {
            run: run.clone(),
            timeouts,
            names: self.names.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            snapshot: Mutex::default(),
            pending: Mutex::default(),
            joined: Mutex::default(),
            tools: Mutex::default(),
            offered: Mutex::default(),
            taken: Mutex::default(),
        });
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(late.clone());
        Some(late)
    }

    /// The run `run_id` ended (its hooks dropped).
    pub(crate) fn ended(&self, run_id: &str) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        runs.retain(|r| r.run.run_id != run_id);
        if runs.len() <= 1 {
            // Logged again the next time runs overlap.
            self.crowded.store(false, Ordering::SeqCst);
        }
    }

    /// A restricted handler registered: it joins the run in progress, when
    /// there is exactly one; else it waits for the next run's snapshot.
    pub(crate) fn registered(&self, h: Registered) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.as_slice() {
            [] => {}
            [run]
                if !run
                    .snapshot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&h.name) =>
            {
                tracing::info!(run_id = %run.run.run_id, handler = %h.name, "a restricted handler registered mid-run: its tools are offered from the next turn");
                run.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(h);
                self.generation.fetch_add(1, Ordering::SeqCst);
            }
            [_] => {}
            _ => {
                if !self.crowded.swap(true, Ordering::SeqCst) {
                    tracing::warn!(handler = %h.name, "several runs of one extension are going (one extension per agent): a handler registered mid-run waits for the next run (logged once)");
                }
            }
        }
    }

    /// The one run in progress, if exactly one.
    fn current(&self) -> Option<Arc<LateRun>> {
        match self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_slice()
        {
            [run] => Some(run.clone()),
            _ => None,
        }
    }
}

/// The tools of restricted handlers that registered while a run was going,
/// offered from the run's next turn: a yoagent `ToolSource` paired with one
/// [`RutisExtension`](crate::RutisExtension) — from
/// [`late_tools`](crate::RutisExtension::late_tools) — installed on the same
/// agent:
///
/// ```no_run
/// # fn demo(bridge: &yoagent_rutis::RutisBridge, agent: yoagent::Agent) -> yoagent::Agent {
/// let extension = bridge
///     .extension()
///     .with_restricted_tools(|handler, tool| Some(format!("{handler}__{tool}")));
/// let late = extension.late_tools();
/// agent.with_extension(extension).with_tool_source(late)
/// # }
/// ```
///
/// - Only **restricted** handlers join a run late (see the
///   [`registry`](crate::registry) docs); others wait for the next run.
/// - Only the extension's **own** runs (not those it serves as a tree
///   extension), and only while **one** is going: one extension per agent.
///   With several, a late handler waits for the next run (logged once).
/// - Outside a run it offers nothing: handlers registered between runs are
///   in the next run's snapshot, offered by the extension.
/// - Named by [`with_restricted_tools`](crate::RutisExtension::with_restricted_tools)
///   like the extension's restricted tools; a name the run already offers is
///   left out (logged).
pub struct LateTools {
    pub(crate) late: Arc<Late>,
    pub(crate) registry: Arc<crate::registry::Registry>,
}

#[async_trait::async_trait]
impl ToolSource for LateTools {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let Some(run) = self.late.current() else {
            return Vec::new();
        };
        let pending = std::mem::take(&mut *run.pending.lock().unwrap_or_else(|e| e.into_inner()));
        for h in pending {
            if !h.gate.is_available() {
                continue;
            }
            let mut offers: Vec<Offer> = h
                .handler
                .static_tools()
                .into_iter()
                .map(|tool| Offer { tool, source: None })
                .collect();
            if h.hooks.tools {
                match call(
                    &h,
                    "tools",
                    run.timeouts.turn,
                    h.handler.tools(run.run.clone()),
                )
                .await
                {
                    Ok(dynamic) => offers.extend(dynamic),
                    Err(Missed::Unavailable(why)) => {
                        tracing::debug!(run_id = %run.run.run_id, "a late handler's tools: {why}");
                        continue;
                    }
                    Err(Missed::Failed(why)) => {
                        tracing::warn!(run_id = %run.run.run_id, "a late handler's tools: {why}; none offered");
                        continue;
                    }
                }
            }
            for Offer { tool, source } in offers {
                let own = tool.name().to_string();
                let Some(name) = offered_name(&run.names, &h, &own) else {
                    tracing::debug!(run_id = %run.run.run_id, handler = %h.name, tool = %own, "left out by the host");
                    continue;
                };
                let taken = run
                    .taken
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&name);
                let mut offered = run.offered.lock().unwrap_or_else(|e| e.into_inner());
                if taken || offered.contains_key(&name) {
                    tracing::warn!(run_id = %run.run.run_id, tool = %name, handler = %h.name, "a late plugin tool's name is taken in this run; left out");
                    continue;
                }
                let (owner, tool) = live(
                    &h,
                    tool,
                    source,
                    name.clone(),
                    &run.run,
                    run.timeouts.policy,
                );
                offered.insert(name, owner);
                drop(offered);
                run.tools
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(tool);
            }
            run.joined.lock().unwrap_or_else(|e| e.into_inner()).push(h);
        }
        let offered = run
            .offered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !offered.is_empty() {
            self.registry.add_offers(&run.run.run_id, offered);
        }
        let tools = run.tools.lock().unwrap_or_else(|e| e.into_inner()).clone();
        tools
    }

    fn generation(&self) -> u64 {
        self.late.generation.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    use tokio_util::sync::CancellationToken;
    use yoagent::extension::{ExtensionError, InputDecision, StopDecision, TurnDecision};
    use yoagent::provider::mock::{MockResponse, MockToolCall};
    use yoagent::provider::{
        MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
    };
    use yoagent::{Agent, AgentEvent, Message, ToolContext, ToolDecision, ToolError, ToolResult};

    use super::*;
    use crate::{Handler, Registry, RutisBridge};

    type Log = Arc<StdMutex<Vec<String>>>;

    /// Records the tool names each request offers.
    struct Recording(MockProvider, Arc<StdMutex<Vec<Vec<String>>>>);

    #[async_trait::async_trait]
    impl StreamProvider for Recording {
        async fn stream(
            &self,
            config: StreamConfig,
            tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
            cancel: CancellationToken,
        ) -> Result<Message, ProviderError> {
            self.1
                .lock()
                .unwrap()
                .push(config.tools.iter().map(|t| t.name.clone()).collect());
            self.0.stream(config, tx, cancel).await
        }
    }

    /// A tool replying `name`; runs `then` (once) when called.
    struct Tool {
        name: String,
        then: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            then: StdMutex::new(None),
        }
    }

    #[async_trait::async_trait]
    impl AgentTool for Tool {
        fn name(&self) -> &str {
            &self.name
        }
        fn label(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "a tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            if let Some(then) = self.then.lock().unwrap().take() {
                then();
            }
            Ok(ToolResult {
                content: vec![yoagent::Content::Text {
                    text: self.name.clone(),
                }],
                details: serde_json::Value::Null,
            })
        }
    }

    fn calls(name: &str) -> MockResponse {
        MockResponse::ToolCalls(vec![MockToolCall {
            name: name.into(),
            arguments: serde_json::json!({}),
            provider_metadata: None,
        }])
    }

    /// A handler with every hook, each logged as `<name> <hook>[ <tool>]`,
    /// and a static tool `<name>_tool`.
    fn logged(name: &str, log: &Log) -> Handler {
        let entry = |hook: &'static str| {
            let (log, name) = (log.clone(), name.to_string());
            move |extra: &str| {
                let line = format!("{name} {hook}{extra}");
                log.lock().unwrap().push(line);
            }
        };
        let (bt, at, bm, oi, os, fi, oe) = (
            entry("before_tool"),
            entry("after_tool"),
            entry("before_model"),
            entry("on_input"),
            entry("on_stop"),
            entry("finish"),
            entry("on_event"),
        );
        Handler::new(name)
            .with_tool(tool(&format!("{name}_tool")))
            .with_before_tool(move |call| {
                bt(&format!(" {}", call.tool));
                ToolDecision::Allow
            })
            .with_after_tool(move |_, _| -> Result<(), ExtensionError> {
                at("");
                Ok(())
            })
            .with_before_model(move |_| {
                bm("");
                TurnDecision::Continue
            })
            .with_on_input(move |_| {
                oi("");
                InputDecision::Pass
            })
            .with_on_stop(move |_| {
                os("");
                StopDecision::Accept
            })
            .with_finish(move |_, _| fi(""))
            .with_on_event(move |_, event| {
                if matches!(event, AgentEvent::AgentEnd { .. }) {
                    oe("");
                }
            })
    }

    fn insert(registry: &Registry, handler: Handler, restricted: bool) {
        let name = handler.name().to_string();
        registry
            .insert(
                name,
                "test".into(),
                Arc::new(handler),
                CancellationToken::new(),
                restricted,
            )
            .unwrap();
    }

    fn lines(log: &Log, who: &str) -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(&format!("{who} ")))
            .cloned()
            .collect()
    }

    async fn go(agent: &mut Agent) {
        let mut rx = agent.prompt("go").await;
        while rx.recv().await.is_some() {}
        agent.finish().await;
    }

    /// A restricted handler gets `tools`, `before_tool` for its own tool
    /// only, and `finish`; an unrestricted one in the same run, every hook.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restricted_handler_gets_only_its_tools_and_finish() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let log = Log::default();
        insert(bridge.registry(), logged("boxed", &log), true);
        insert(bridge.registry(), logged("plain", &log), false);
        let seen = Arc::default();
        let provider = Recording(
            MockProvider::new(vec![
                calls("boxed__boxed_tool"),
                calls("plain_tool"),
                MockResponse::Text("done".into()),
            ]),
            seen,
        );
        let mut agent =
            Agent::from_provider(provider, ModelConfig::mock()).with_extension(bridge.extension());
        go(&mut agent).await;
        assert_eq!(
            lines(&log, "boxed"),
            ["boxed before_tool boxed_tool", "boxed finish"]
        );
        let plain = lines(&log, "plain");
        for hook in [
            "plain on_input",
            "plain before_model",
            "plain before_tool boxed__boxed_tool",
            "plain before_tool plain_tool",
            "plain after_tool",
            "plain on_stop",
            "plain finish",
            "plain on_event",
        ] {
            assert!(plain.iter().any(|l| l == hook), "{hook}: {plain:?}");
        }
        let info = bridge.registry().handlers();
        let boxed = info.iter().find(|h| h.name() == "boxed").unwrap();
        assert!(boxed.restricted());
        assert_eq!(boxed.hooks(), ["before_tool", "finish"]);
        root.shutdown().await.unwrap();
    }

    /// The host names a restricted handler's tools (or leaves one out); the
    /// plugin is still called with its own name. Other handlers' tools keep
    /// theirs.
    #[tokio::test(flavor = "multi_thread")]
    async fn restricted_tools_are_named_by_the_host() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let log = Log::default();
        insert(
            bridge.registry(),
            logged("boxed", &log).with_tool(tool("hidden")),
            true,
        );
        insert(bridge.registry(), logged("plain", &log), false);
        let seen: Arc<StdMutex<Vec<Vec<String>>>> = Arc::default();
        let provider = Recording(
            MockProvider::new(vec![
                calls("boxed__boxed_tool"),
                MockResponse::Text("done".into()),
            ]),
            seen.clone(),
        );
        let extension = bridge.extension().with_restricted_tools(|handler, tool| {
            (tool != "hidden").then(|| format!("{handler}__{tool}"))
        });
        let mut agent =
            Agent::from_provider(provider, ModelConfig::mock()).with_extension(extension);
        go(&mut agent).await;
        let mut offered = seen.lock().unwrap()[0].clone();
        offered.sort();
        assert_eq!(offered, ["boxed__boxed_tool", "plain_tool"]);
        assert_eq!(
            lines(&log, "boxed"),
            ["boxed before_tool boxed_tool", "boxed finish"],
            "the plugin sees its own name"
        );
        root.shutdown().await.unwrap();
    }

    /// `require_policy`: a restricted handler's `before_tool` is no policy.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restricted_before_tool_is_not_a_policy() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let log = Log::default();
        insert(bridge.registry(), logged("boxed", &log), true);
        let provider = Recording(
            MockProvider::new(vec![
                calls("boxed__boxed_tool"),
                MockResponse::Text("done".into()),
            ]),
            Arc::default(),
        );
        let mut agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_extension(bridge.extension().require_policy());
        go(&mut agent).await;
        assert!(
            !lines(&log, "boxed")
                .iter()
                .any(|l| l.contains("before_tool")),
            "denied before any plugin judged it"
        );
        root.shutdown().await.unwrap();
    }

    /// Mid-run: a restricted handler registered while the run goes is
    /// offered from the next turn (named by the host), judges its own calls,
    /// is finished with the run, and `offered_by` names it. An unrestricted
    /// one waits for the next run.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restricted_handler_registered_mid_run_joins_it() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let log = Log::default();
        let registry = bridge.registry().clone();
        let (late_log, owners) = (log.clone(), Arc::new(StdMutex::new(None)));
        let load = tool("load");
        *load.then.lock().unwrap() = Some(Box::new(move || {
            insert(&registry, logged("late", &late_log), true);
            insert(&registry, logged("eager", &late_log), false);
        }));
        let seen: Arc<StdMutex<Vec<Vec<String>>>> = Arc::default();
        let provider = Recording(
            MockProvider::new(vec![
                calls("load"),
                calls("x__late_tool"),
                MockResponse::Text("done".into()),
            ]),
            seen.clone(),
        );
        let extension = bridge
            .extension()
            .with_restricted_tools(|_, tool| Some(format!("x__{tool}")));
        let late = extension.late_tools();
        let registry = bridge.registry().clone();
        let owners_seen = owners.clone();
        let probe = Handler::new("probe").with_before_tool(move |call| {
            if call.tool == "x__late_tool" {
                *owners_seen.lock().unwrap() =
                    registry.offered_by(&call.run.run_id, "x__late_tool");
            }
            ToolDecision::Allow
        });
        insert(bridge.registry(), probe, false);
        let mut agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_tools(vec![Box::new(load)])
            .with_extension(extension)
            .with_tool_source(late);
        go(&mut agent).await;
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(!seen[0].contains(&"x__late_tool".to_string()));
        assert!(seen[1].contains(&"x__late_tool".to_string()), "{seen:?}");
        assert!(
            !seen[1].iter().any(|t| t.contains("eager")),
            "an unrestricted handler waits for the next run: {seen:?}"
        );
        assert_eq!(
            lines(&log, "late"),
            ["late before_tool late_tool", "late finish"]
        );
        assert!(
            lines(&log, "eager").is_empty(),
            "{:?}",
            lines(&log, "eager")
        );
        let owner = owners.lock().unwrap().clone().expect("offered_by names it");
        assert_eq!(owner.handler(), "late");
        root.shutdown().await.unwrap();
    }

    /// A restricted handler offering a tool named like the agent's own never
    /// judges the agent's calls: its check runs inside its own tool, which
    /// yoagent dropped for the agent's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restricted_handler_never_judges_a_tool_sharing_its_name() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let log = Log::default();
        insert(
            bridge.registry(),
            logged("boxed", &log).with_tool(tool("shell")),
            true,
        );
        let provider = Recording(
            MockProvider::new(vec![calls("shell"), MockResponse::Text("done".into())]),
            Arc::default(),
        );
        let extension = bridge
            .extension()
            .with_restricted_tools(|_, tool| Some(tool.to_string()));
        let mut agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_tools(vec![Box::new(tool("shell"))])
            .with_extension(extension);
        go(&mut agent).await;
        assert_eq!(lines(&log, "boxed"), ["boxed finish"]);
        root.shutdown().await.unwrap();
    }

    /// A restricted handler's check of its own call gets the call alone: no
    /// request text, no conversation.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restricted_handler_never_sees_the_users_text() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let seen: Log = Log::default();
        let record = seen.clone();
        let boxed = Handler::new("boxed")
            .with_tool(tool("t"))
            .with_messages()
            .with_before_tool(move |call| {
                record.lock().unwrap().push(format!(
                    "{:?} {:?} {}",
                    call.user_request,
                    call.latest_user_text,
                    call.messages.is_some()
                ));
                ToolDecision::Allow
            });
        insert(bridge.registry(), boxed, true);
        let provider = Recording(
            MockProvider::new(vec![calls("boxed__t"), MockResponse::Text("done".into())]),
            Arc::default(),
        );
        let mut agent =
            Agent::from_provider(provider, ModelConfig::mock()).with_extension(bridge.extension());
        go(&mut agent).await;
        assert_eq!(*seen.lock().unwrap(), ["None None false"]);
        root.shutdown().await.unwrap();
    }

    /// A restricted handler whose `tools` fails offers nothing; it does not
    /// fail a required extension's run.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_restricted_handler_does_not_fail_the_run() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let broken = Handler::new("broken").with_tools(|_| panic!("tools broke"));
        insert(bridge.registry(), broken, true);
        let provider = Recording(
            MockProvider::new(vec![MockResponse::Text("done".into())]),
            Arc::default(),
        );
        let mut agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_extension(bridge.extension().required());
        go(&mut agent).await;
        let last = agent.messages().last().cloned();
        assert!(
            matches!(
                &last,
                Some(yoagent::AgentMessage::Llm(Message::Assistant {
                    stop_reason: yoagent::StopReason::Stop,
                    ..
                }))
            ),
            "{last:?}"
        );
        root.shutdown().await.unwrap();
    }

    /// A trusted registration evicts a restricted handler holding its name;
    /// a restricted one never evicts a trusted one. Static tool names clash
    /// only between trusted handlers: a restricted handler's are offered
    /// under other names.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_trusted_handler_evicts_a_restricted_squatter() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let registry = bridge.registry();
        insert(registry, Handler::new("policy").with_tool(tool("t")), true);
        insert(registry, Handler::new("policy"), false);
        insert(registry, Handler::new("other").with_tool(tool("u")), true);
        insert(registry, Handler::new("owner").with_tool(tool("u")), false);
        let info = registry.handlers();
        insert(registry, Handler::new("also").with_tool(tool("u")), true);
        let names: Vec<(&str, bool)> = info.iter().map(|h| (h.name(), h.restricted())).collect();
        assert_eq!(
            names,
            [("policy", false), ("other", true), ("owner", false)]
        );
        assert_eq!(
            registry.handlers().len(),
            4,
            "two restricted `u` tools coexist"
        );
        let refused = registry.insert(
            "policy".into(),
            "test".into(),
            Arc::new(Handler::new("policy")),
            CancellationToken::new(),
            true,
        );
        assert!(
            refused.is_err(),
            "a restricted handler never takes a trusted name"
        );
        root.shutdown().await.unwrap();
    }

    /// Two restricted handlers registered mid-run, on different turns: both
    /// are offered from then on (the source's list grows), a tool the host
    /// leaves out is not, every late handler is finished, and `offered_by`
    /// forgets the run's late tools when it ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn late_handlers_accumulate_and_are_forgotten_with_the_run() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let log = Log::default();
        let (registry, first_log) = (bridge.registry().clone(), log.clone());
        let first = tool("first");
        *first.then.lock().unwrap() = Some(Box::new(move || {
            insert(
                &registry,
                logged("one", &first_log).with_tool(tool("hidden")),
                true,
            )
        }));
        let (registry, second_log) = (bridge.registry().clone(), log.clone());
        let second = tool("second");
        *second.then.lock().unwrap() = Some(Box::new(move || {
            insert(&registry, logged("two", &second_log), true)
        }));
        let seen: Arc<StdMutex<Vec<Vec<String>>>> = Arc::default();
        let provider = Recording(
            MockProvider::new(vec![
                calls("first"),
                calls("second"),
                MockResponse::Text("done".into()),
            ]),
            seen.clone(),
        );
        let extension = bridge
            .extension()
            .with_restricted_tools(|h, t| (t != "hidden").then(|| format!("{h}__{t}")));
        let late = extension.late_tools();
        let mut agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_tools(vec![Box::new(first), Box::new(second)])
            .with_extension(extension)
            .with_tool_source(late);
        let run_id: Arc<StdMutex<Option<String>>> = Arc::default();
        let record = run_id.clone();
        insert(
            bridge.registry(),
            Handler::new("ids").with_finish(move |_, run| {
                *record.lock().unwrap() = Some(run.run_id.clone());
            }),
            false,
        );
        go(&mut agent).await;
        let seen = seen.lock().unwrap().clone();
        let has = |i: usize, t: &str| seen[i].iter().any(|s| s == t);
        assert!(
            has(1, "one__one_tool") && !has(1, "two__two_tool"),
            "{seen:?}"
        );
        assert!(
            has(2, "one__one_tool") && has(2, "two__two_tool"),
            "{seen:?}"
        );
        assert!(
            !seen.iter().flatten().any(|t| t.contains("hidden")),
            "{seen:?}"
        );
        assert_eq!(lines(&log, "one"), ["one finish"]);
        assert_eq!(lines(&log, "two"), ["two finish"]);
        let run_id = run_id.lock().unwrap().clone().unwrap();
        assert!(bridge
            .registry()
            .offered_by(&run_id, "one__one_tool")
            .is_none());
        root.shutdown().await.unwrap();
    }

    /// Cancelling the run ends a restricted handler's check of its own call:
    /// the run does not wait for it, and the tool does not run.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_run_does_not_wait_for_a_restricted_check() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let slow = Tool {
            name: "slow".into(),
            then: StdMutex::new(Some(Box::new(move || flag.store(true, Ordering::SeqCst)))),
        };
        let boxed = Handler::new("boxed")
            .with_tool(slow)
            .with_before_tool_async(|_| async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(ToolDecision::Allow)
            });
        insert(bridge.registry(), boxed, true);
        let provider = Recording(
            MockProvider::new(vec![
                calls("boxed__slow"),
                MockResponse::Text("done".into()),
            ]),
            Arc::default(),
        );
        let mut agent =
            Agent::from_provider(provider, ModelConfig::mock()).with_extension(bridge.extension());
        let mut rx = agent.prompt("go").await;
        let started = std::time::Instant::now();
        while let Some(event) = rx.recv().await {
            if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                agent.abort();
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), agent.finish())
            .await
            .expect("the run ends");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!ran.load(Ordering::SeqCst), "the tool never ran");
        root.shutdown().await.unwrap();
    }

    /// Offers one tool from an extension of its own.
    #[derive(Clone)]
    struct Offers(&'static str);

    #[async_trait::async_trait]
    impl yoagent::RunHooks for Offers {
        async fn tools(
            &mut self,
            _: &yoagent::extension::RunContext<'_>,
        ) -> Vec<Arc<dyn AgentTool>> {
            vec![Arc::new(tool(self.0))]
        }
    }

    /// A late restricted tool never takes the name of a tool another
    /// extension offers (as a tool source it would win over it).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_tool_never_takes_another_extensions_name() {
        let root = rutis::Ctx::root().unwrap();
        let bridge = RutisBridge::install(&root).unwrap();
        let log = Log::default();
        let (registry, late_log) = (bridge.registry().clone(), log.clone());
        let load = tool("load");
        *load.then.lock().unwrap() = Some(Box::new(move || {
            insert(
                &registry,
                logged("acme", &late_log).with_tool(tool("deploy")),
                true,
            )
        }));
        let seen: Arc<StdMutex<Vec<Vec<String>>>> = Arc::default();
        let provider = Recording(
            MockProvider::new(vec![
                calls("load"),
                calls("acme__deploy"),
                MockResponse::Text("done".into()),
            ]),
            seen.clone(),
        );
        let extension = bridge.extension();
        let late = extension.late_tools();
        let mut agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_tools(vec![Box::new(load)])
            .with_extension(extension)
            .with_extension(yoagent::extension::ClonedHooks::new(
                "other",
                Offers("acme__deploy"),
            ))
            .with_tool_source(late);
        go(&mut agent).await;
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen[1].iter().filter(|t| *t == "acme__deploy").count(),
            1,
            "{seen:?}"
        );
        assert!(
            !lines(&log, "acme")
                .iter()
                .any(|l| l.contains("before_tool")),
            "the other extension's tool ran, not the plugin's: {:?}",
            lines(&log, "acme")
        );
        root.shutdown().await.unwrap();
    }
}
