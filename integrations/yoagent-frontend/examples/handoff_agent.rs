//! EXPERIMENTAL. A personal handoff agent: hand it a task and walk away.
//!
//! - It plans with a pi extension, unchanged (`@juicesharp/rpiv-todo`'s
//!   `todo` tool; the `todo-plan` UI plugin draws the plan in the browser),
//!   researches with DSH's free web search, and works with yoagent's file
//!   and shell tools in the directory it was started in.
//! - It asks without waiting: `ask_owner` puts a question to every frontend
//!   — kept open while nobody is there (`Session::ask_later`) — and returns
//!   at once, so the agent carries on with what does not depend on it. The
//!   answer, whenever it comes, is sent in as a steer message: picked up by
//!   the run in progress, or starting a new run when the agent had stopped.
//! - Guardrails: a pi extension asks before dangerous commands (those do
//!   wait); every run needs that policy (`require_policy`); runs are capped
//!   in turns and minutes, and in dollars when the model has a price.
//! - Every run ends with a handoff report (Done / Waiting on you /
//!   Decisions / Next — the agent is sent back until it writes one), and
//!   the host adds what the model cannot make up: the files changed, the
//!   tokens, tool calls and time, the questions still open.
//!
//! ```bash
//! scripts/try-handoff.sh "migrate the config to TOML, keep tests green"   # from the repo root
//! cargo run --example handoff_agent -- --live "…"     # DeepSeek (DEEPSEEK_API_KEY)
//! cargo run --example handoff_agent -- --phone "…"    # serve on the LAN, for a phone
//! cargo run --example handoff_agent -- --demo         # headless, scripted (CI)
//! ```
//!
//! `--web ADDR` (default 127.0.0.1:8787), `--no-dsh`, `--max-minutes N`
//! (30), `--budget-usd X` (2; priced models only). Without a task, give it
//! one in the page. `--phone` serves on every interface: anyone on the
//! network with the printed URL (it carries the token) drives the agent.
//!
//! Setup: `scripts/try-coding-agent.sh --setup-only` (Node 24+, npm).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rutis::Ctx;
use serde_json::{json, Value as Json};
use yoagent::context::ExecutionLimits;
use yoagent::extension::{
    Extension, ExtensionError, RunContext, RunHooks, RunOutcome, StopContext, StopDecision,
};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{prices, MockProvider, ModelConfig};
use yoagent::tools::default_tools;
use yoagent::{
    Agent, AgentEvent, AgentMessage, AgentTool, Content, Message, ToolContext, ToolError,
    ToolResult,
};
use yoagent_frontend::host::{PluginHost, Row};
use yoagent_frontend::{
    services, session, web, ClientMessage, NoticeLevel, ServerMessage, Session, UiRequest,
};
use yoagent_rutis::RutisBridge;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The sections every run's last message must have.
const REPORT: [&str; 4] = ["Done", "Waiting on you", "Decisions", "Next"];

fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn rutis_plugins() -> PathBuf {
    here().join("../yoagent-rutis/plugins")
}

struct Options {
    key: Option<String>,
    addr: SocketAddr,
    phone: bool,
    demo: bool,
    dsh: bool,
    task: Option<String>,
    max_minutes: u64,
    budget_usd: f64,
}

fn main() -> Result<(), BoxError> {
    let mut options = Options {
        key: None,
        addr: "127.0.0.1:8787".parse()?,
        phone: false,
        demo: false,
        dsh: rutis_plugins().join("dsh/node_modules").exists(),
        task: None,
        max_minutes: 30,
        budget_usd: 2.0,
    };
    let mut live = false;
    let mut words = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--live" => live = true,
            "--demo" => options.demo = true,
            "--no-dsh" => options.dsh = false,
            "--phone" => {
                options.phone = true;
                options.addr = "0.0.0.0:8787".parse()?;
            }
            "--web" => options.addr = args.next().ok_or("--web needs ADDR")?.parse()?,
            "--max-minutes" => {
                options.max_minutes = args.next().ok_or("--max-minutes needs N")?.parse()?
            }
            "--budget-usd" => {
                options.budget_usd = args.next().ok_or("--budget-usd needs X")?.parse()?
            }
            flag if flag.starts_with("--") => return Err(format!("unknown argument {flag}").into()),
            word => words.push(word.to_owned()),
        }
    }
    options.task = (!words.is_empty()).then(|| words.join(" "));
    for dir in [here().join("plugins"), rutis_plugins().join("pi")] {
        if !dir.join("node_modules").exists() {
            return Err(format!(
                "run `npm ci` in {} first (or scripts/try-coding-agent.sh --setup-only)",
                dir.display()
            )
            .into());
        }
    }
    if live {
        match std::env::var("DEEPSEEK_API_KEY") {
            Ok(key) if !key.trim().is_empty() => options.key = Some(key.trim().to_owned()),
            _ => return Err("--live needs DEEPSEEK_API_KEY".into()),
        }
    }
    if std::env::var_os("NODE_OPTIONS").is_none() {
        std::env::set_var("NODE_OPTIONS", "--no-warnings");
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("warn,yoagent::provider::registry=error")
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let root = Ctx::root()?;
            let result = run(&root, options).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Err(e) = root.shutdown().await {
                match &result {
                    Ok(()) => return Err(e.into()),
                    Err(_) => eprintln!("shutting the plugins down also failed: {e}"),
                }
            }
            result
        })
}

async fn run(root: &Ctx, options: Options) -> Result<(), BoxError> {
    let cwd = std::env::current_dir()?;
    // The demo ends itself with `Quit`: the agent's own tools hold session
    // handles, so the driver would not end when the demo drops its own.
    let (session, driver) = Session::new(options.demo);
    let bridge = RutisBridge::install(root)?;
    services::provide(root, &session)?;

    let mut builder = PluginHost::builder()
        .node("ui", here().join("plugins"))
        .node("pi", rutis_plugins().join("pi"));
    if options.dsh {
        builder = builder.node("dsh", rutis_plugins().join("dsh"));
    }
    let mut host = builder
        .share("yoagent")
        .share(services::FRONTEND)
        .share(services::UI)
        .start(root)
        .await?;
    let mut rows = vec![
        // pi extensions, unchanged: the plan, and approval of dangerous commands.
        Row::new(
            "pi",
            rutis_plugins()
                .join("pi/pi-extensions-adapter.ts")
                .display(),
        )
        .runtime("pi")
        .config(json!({
            "extensions": [
                here().join("plugins/node_modules/@juicesharp/rpiv-todo/index.ts"),
                here().join("plugins/pi-extensions/confirm-dangerous.ts"),
            ],
            "cwd": cwd,
        })),
        Row::new(
            "pi-host-ui",
            rutis_plugins().join("pi/host-ui.ts").display(),
        )
        .runtime("pi"),
        Row::new("todo-plan", here().join("plugins/todo-plan.ts").display()).runtime("ui"),
    ];
    if options.dsh {
        let dsh = |id: &str, name: &str| Row::new(id, name).runtime("dsh");
        rows.extend([
            dsh("dsh-web", "@deepseek-ai/dsh-web"),
            dsh("dsh-system-prompt", "@deepseek-ai/dsh-system-prompt"),
            dsh("dsh-tools", "@deepseek-ai/dsh-tools"),
            dsh("dsh-free-search", "dsh-free-search").config(
                json!({ "provider": "bing", "disabledEngines": [], "bingMarket": "en-US" }),
            ),
            Row::new(
                "dsh",
                rutis_plugins().join("dsh/dsh-tools-adapter.ts").display(),
            )
            .runtime("dsh"),
            Row::new(
                "dsh-host-dialogs",
                rutis_plugins().join("dsh/host-dialogs.ts").display(),
            )
            .runtime("dsh"),
        ]);
    }
    host.load(rows).await?;
    // A TypeScript row runs before its async start-up is done: wait for the
    // handlers the agent depends on, the pi policy above all.
    let mut needed = vec!["pi-extensions"];
    if options.dsh {
        needed.push("dsh-tools");
    }
    let registry = bridge.registry().clone();
    tokio::time::timeout(Duration::from_secs(60), async {
        while !needed
            .iter()
            .all(|name| registry.handlers().iter().any(|h| h.name() == *name))
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        format!(
            "plugins did not register their handlers: {:?}",
            host.status()
        )
    })?;

    // Prices are opt-in; a model with none gets no dollar cap (turns and minutes still apply).
    prices::enable_bundled();
    let config = match &options.key {
        Some(_) => ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"),
        None => ModelConfig::mock(),
    };
    let budget = yoagent::extension::Budget::for_model(options.budget_usd, &config)
        .map(|budget| budget.across_runs());
    let open = Arc::new(AtomicUsize::new(0));
    let mut agent = match &options.key {
        Some(key) => Agent::from_config(config).with_api_key(key.clone()),
        None => Agent::from_provider(scripted(), config),
    }
    .with_system_prompt(system_prompt(&cwd))
    .with_tools({
        // `with_tools` sets the whole list: yoagent's tools and `ask_owner`.
        let mut tools = default_tools();
        tools.push(Box::new(AskOwner {
            session: session.clone(),
            next: Arc::default(),
            open: open.clone(),
        }));
        tools
    })
    .with_execution_limits(
        ExecutionLimits::default()
            .with_max_turns(60)
            .with_max_duration(Duration::from_secs(options.max_minutes * 60)),
    )
    // Questions from pi's approval wait up to the frontends' timeout.
    .with_extension(
        bridge
            .extension()
            .require_policy()
            .with_policy_timeout(Some(session::UI_TIMEOUT + Duration::from_secs(30))),
    )
    .with_extension(Handoff {
        session: session.clone(),
        // Where the handoff started: the facts compare against it, so commits the agent made count.
        start: git(&cwd, &["rev-parse", "HEAD"]),
        cwd: cwd.clone(),
        open,
    });
    let priced = budget.is_some();
    if let Some(budget) = budget {
        agent = agent.with_extension(budget);
    }

    if options.demo {
        return demo(session, driver, agent, options.task).await;
    }
    let served = web::serve(session.clone(), options.addr).await?;
    let url = match (options.phone, lan_ip()) {
        (true, Some(ip)) => format!("http://{ip}:{}/?t={}", served.addr().port(), served.token()),
        _ => served.url(),
    };
    println!("handoff agent: {url}");
    println!(
        "  limits: {} min per run{}  (Ctrl+C to stop)",
        options.max_minutes,
        if priced {
            format!(", ${:.2} in all", options.budget_usd)
        } else {
            ", no dollar cap (this model has no price)".into()
        }
    );
    if let Some(task) = options.task {
        session.send(ClientMessage::Prompt { text: task });
    }
    tokio::select! {
        _ = driver.run(agent) => {}
        stop = tokio::signal::ctrl_c() => {
            if let Err(e) = stop {
                eprintln!("cannot listen for Ctrl+C ({e}); stopping");
            }
        }
    }
    served.stop();
    Ok(())
}

fn system_prompt(cwd: &std::path::Path) -> String {
    format!(
        "You are the owner's handoff agent, working in {cwd}. The owner gave you a task and \
         is away. Work on your own:\n\
         - Plan with the `todo` tool: create the steps first, mark each in progress and done.\n\
         - When a decision is the owner's, call `ask_owner` and keep going with work that does \
         not depend on it. Never wait for an answer: it arrives later as a message starting \
         with `[Owner's answer to #N]` (or `[Owner skipped #N]`: then decide yourself and say \
         so). If everything left depends on an answer, stop.\n\
         - Prefer reversible steps; dangerous commands ask the owner and may be refused.\n\
         - End every run with a handoff report in Markdown, with these sections: {sections}. \
         Under Waiting on you, list the open questions by number.",
        cwd = cwd.display(),
        sections = REPORT.map(|s| format!("## {s}")).join(", "),
    )
}

/// Puts a question to the owner and returns at once; the answer comes back
/// as a steer message.
struct AskOwner {
    session: Session,
    next: Arc<AtomicU64>,
    /// Questions not answered yet (shared with the report).
    open: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentTool for AskOwner {
    fn name(&self) -> &str {
        "ask_owner"
    }
    fn label(&self) -> &str {
        "Ask the owner"
    }
    fn description(&self) -> &str {
        "Ask the owner a question they decide, without waiting: this returns at once. Continue \
         with work that does not depend on the answer; it arrives later as a message. Give \
         `options` when the answer is one of a few choices."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question, on its own." },
                "why": { "type": "string", "description": "What depends on it, briefly." },
                "options": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, params: Json, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let question = params["question"]
            .as_str()
            .filter(|q| !q.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("`question` is required".into()))?
            .to_owned();
        let why = params["why"].as_str().unwrap_or_default().to_owned();
        let options: Vec<String> = params["options"]
            .as_array()
            .map(|o| {
                o.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let title = format!("#{n} {question}");
        let request = if options.len() >= 2 {
            UiRequest::Select {
                title,
                message: why,
                options,
                multiple: false,
            }
        } else {
            UiRequest::Input {
                title,
                message: why,
                placeholder: "Your answer".into(),
                value: String::new(),
            }
        };
        let (session, open) = (self.session.clone(), self.open.clone());
        open.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let answer = session.ask_later(request).await;
            open.fetch_sub(1, Ordering::Relaxed);
            let text = match answer.as_str().map(str::trim) {
                Some(answer) if !answer.is_empty() => {
                    format!("[Owner's answer to #{n}] {question}\n{answer}")
                }
                _ => format!("[Owner skipped #{n}] {question}"),
            };
            // Mid-run it is picked up between tool batches; when the agent
            // had stopped, it starts a new run.
            session.send(ClientMessage::Steer { text });
        });
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!(
                    "Asked the owner as #{n}. Do not wait: continue with what does not depend on \
                     it. The answer will arrive as a message starting with [Owner's answer to #{n}]."
                ),
            }],
            details: json!({ "question": n }),
        })
    }
}

/// Every run ends with the handoff report, and the facts the model cannot
/// make up, told to the frontends.
struct Handoff {
    session: Session,
    cwd: PathBuf,
    /// The commit the handoff started from (`None`: not a git repository).
    start: Option<String>,
    open: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Extension for Handoff {
    fn name(&self) -> &str {
        "handoff"
    }
    async fn start_run(&self, _run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(HandoffRun {
            session: self.session.clone(),
            cwd: self.cwd.clone(),
            start: self.start.clone(),
            open: self.open.clone(),
            started: Instant::now(),
            counts: Mutex::default(),
        }))
    }
}

struct HandoffRun {
    session: Session,
    cwd: PathBuf,
    start: Option<String>,
    open: Arc<AtomicUsize>,
    started: Instant,
    /// Tokens in and out, tool calls.
    counts: Mutex<(u64, u64, usize)>,
}

#[async_trait::async_trait]
impl RunHooks for HandoffRun {
    fn on_event(&self, event: &AgentEvent) {
        let mut counts = self.counts.lock().unwrap();
        match event {
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(Message::Assistant { usage, .. }),
            } => {
                counts.0 += usage.input + usage.cache_read;
                counts.1 += usage.output;
            }
            AgentEvent::ToolExecutionEnd { .. } => counts.2 += 1,
            _ => {}
        }
    }

    async fn on_stop(&mut self, stop: &StopContext<'_>) -> StopDecision {
        let Message::Assistant { content, .. } = stop.answer else {
            return StopDecision::Accept;
        };
        let text: String = content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.to_lowercase()),
                _ => None,
            })
            .collect();
        let missing: Vec<&str> = REPORT
            .into_iter()
            .filter(|s| !text.contains(&s.to_lowercase()))
            .collect();
        if missing.is_empty() {
            StopDecision::Accept
        } else {
            StopDecision::Continue(format!(
                "End with the handoff report: sections {}. Missing: {}.",
                REPORT.map(|s| format!("## {s}")).join(", "),
                missing.join(", ")
            ))
        }
    }

    async fn finish(&mut self, _outcome: &RunOutcome) {
        let (tokens_in, tokens_out, tools) = *self.counts.lock().unwrap();
        // Since the handoff started: committed and uncommitted changes alike.
        let changed = match &self.start {
            Some(start) => {
                let diff = git(&self.cwd, &["diff", "--shortstat", start]).unwrap_or_default();
                let commits = git(
                    &self.cwd,
                    &["rev-list", "--count", &format!("{start}..HEAD")],
                )
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0);
                let diff = if diff.is_empty() {
                    "no changes".into()
                } else {
                    diff
                };
                format!("since the handoff: {diff}, {commits} commit(s)")
            }
            None => "not a git repository".into(),
        };
        let new = git(&self.cwd, &["ls-files", "--others", "--exclude-standard"])
            .map(|s| s.lines().count())
            .unwrap_or(0);
        let open = self.open.load(Ordering::Relaxed);
        self.session.notice(
            NoticeLevel::Info,
            format!(
                "Handoff facts: {changed}{}; {tokens_in} tokens in, {tokens_out} out; {tools} tool \
                 call(s); {:.0}s; {open} question(s) waiting on you.",
                if new > 0 { format!(", {new} new file(s)") } else { String::new() },
                self.started.elapsed().as_secs_f64(),
            ),
        );
    }
}

fn git(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// This machine's address on the local network (no packet is sent).
fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

fn call(name: &str, arguments: Json) -> MockToolCall {
    MockToolCall {
        provider_metadata: None,
        name: name.into(),
        arguments,
    }
}

/// Without `--live`: a plan, a question, work that does not depend on it, a
/// report; then, once the owner answered, the rest.
fn scripted() -> MockProvider {
    MockProvider::new(vec![
        MockResponse::ToolCalls(vec![call(
            "todo",
            json!({ "action": "create", "subject": "Move the config loader to TOML" }),
        )]),
        MockResponse::ToolCalls(vec![
            call(
                "ask_owner",
                json!({
                    "question": "Keep the old YAML loader for one release?",
                    "why": "Deleting it breaks configs not yet migrated.",
                    "options": ["Keep it", "Delete it"]
                }),
            ),
            call("list_files", json!({ "path": "." })),
        ]),
        MockResponse::Text(
            "## Done\n- Planned the migration; listed the project.\n\
             ## Waiting on you\n- #1 Keep the old YAML loader for one release?\n\
             ## Decisions\n- None yet.\n## Next\n- Finish once #1 is answered."
                .into(),
        ),
        MockResponse::ToolCalls(vec![call(
            "todo",
            json!({ "action": "update", "id": 1, "status": "completed" }),
        )]),
        MockResponse::Text(
            "## Done\n- Moved the loader to TOML.\n## Waiting on you\n- Nothing.\n\
             ## Decisions\n- Deleted the YAML loader, as you answered.\n## Next\n- Nothing."
                .into(),
        ),
    ])
}

/// `--demo`: no browser; a frontend plays the owner — it waits for the run
/// to stop, then answers the question, which starts the second run.
async fn demo(
    session: Session,
    driver: yoagent_frontend::Driver,
    agent: Agent,
    task: Option<String>,
) -> Result<(), BoxError> {
    let mut owner = session.connect();
    let running = tokio::spawn(driver.run(agent));
    let task =
        task.unwrap_or_else(|| "Migrate the config loader to TOML; keep tests green.".into());
    println!("> {task}");
    session.send(ClientMessage::Prompt { text: task });
    let mut question = None;
    let mut ended = 0;
    let mut facts = Vec::new();
    while ended < 2 {
        let message = tokio::time::timeout(Duration::from_secs(120), owner.messages.recv())
            .await
            .map_err(|_| "the demo stalled")?
            .ok_or("the session ended early")?;
        match message {
            ServerMessage::Event { event, .. } => match *event {
                AgentEvent::ToolExecutionEnd {
                    tool_name,
                    is_error,
                    result,
                    ..
                } => {
                    let why = if is_error {
                        let text: String = result
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                Content::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        format!(": {}", text.chars().take(160).collect::<String>())
                    } else {
                        String::new()
                    };
                    println!("  {} {tool_name}{why}", if is_error { "✗" } else { "✓" });
                }
                AgentEvent::MessageEnd {
                    message: AgentMessage::Llm(Message::Assistant { content, .. }),
                } => {
                    for c in &content {
                        if let Content::Text { text } = c {
                            if !text.trim().is_empty() {
                                println!("{}", text.trim());
                            }
                        }
                    }
                }
                _ => {}
            },
            ServerMessage::UiRequest { id, request } => {
                println!("  ? (to the owner) {}", request.title());
                question = Some(id);
            }
            ServerMessage::Notice { message, .. } => {
                println!("  [{message}]");
                facts.push(message);
            }
            ServerMessage::RunEnd { outcome, error, .. } => {
                ended += 1;
                println!(
                    "  — run {ended} ended: {outcome:?}{}",
                    error.map(|e| format!(" ({e})")).unwrap_or_default()
                );
                if ended == 1 {
                    // The owner comes back, and answers.
                    let id = question.ok_or("the agent asked the owner nothing")?;
                    println!("  ! (the owner answers #1) Delete it");
                    session.send(ClientMessage::UiResponse {
                        id,
                        value: json!("Delete it"),
                    });
                }
            }
            _ => {}
        }
    }
    drop(owner);
    session.send(ClientMessage::Quit);
    let agent = tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .map_err(|_| "the session did not end")??;
    let answered = agent.messages().iter().any(|m| {
        matches!(m, AgentMessage::Llm(Message::User { content, .. })
            if content.iter().any(|c| matches!(c, Content::Text { text } if text.starts_with("[Owner's answer to #1]"))))
    });
    if !answered {
        return Err("the owner's answer never reached the agent".into());
    }
    if facts
        .iter()
        .filter(|f| f.starts_with("Handoff facts"))
        .count()
        != 2
    {
        return Err(format!("expected the facts after each run: {facts:?}").into());
    }
    println!("ok: asked without waiting, answered later, two reports");
    Ok(())
}
