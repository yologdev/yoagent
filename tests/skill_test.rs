//! `skills/yoagent/SKILL.md` and `llms.txt` stay true:
//! - the skill loads through yoagent's own `SkillSet`;
//! - every repository path both cite exists **in the published crate**
//!   (separate crates under `integrations/` must be linked as URLs), and
//!   every `#anchor` they cite is a heading of its target;
//! - every line of the skill's ```rust recipes appears in this file, where
//!   the recipes are compiled, so a recipe edited only in the skill fails;
//! - the API names the skill mentions only in prose exist (`prose_names`).

use std::path::Path;

use yoagent::context::ExecutionLimits;
use yoagent::extension::{Budget, ClonedHooks, RunHooks, StopContext, StopDecision, TurnDecision};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::{
    Agent, AgentMessage, Content, Message, SkillSet, SubAgentTool, ToolCallRequest, ToolDecision,
};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(file: &str) -> String {
    std::fs::read_to_string(root().join(file)).unwrap()
}

const FILES: [&str; 2] = ["skills/yoagent/SKILL.md", "llms.txt"];

#[test]
fn the_skill_loads_with_yoagents_own_loader() {
    let skills = SkillSet::load(&[root().join("skills")]).expect("skills/ loads");
    let skill = skills
        .skills()
        .iter()
        .find(|s| s.name == "yoagent")
        .expect("a skill named yoagent");
    assert!(skill.description.starts_with("Use when building"));
}

/// Repository paths cited in backticks or as Markdown link targets, with
/// any `#anchor`: `(path, anchor)`.
fn cited_paths(text: &str) -> Vec<(String, Option<String>)> {
    let prefixes = [
        "docs/",
        "integrations/",
        "skills/",
        "src/",
        "tests/",
        "examples/",
        "CHANGELOG.md",
        "llms.txt",
        "README",
    ];
    let mut found = Vec::new();
    for piece in text.split(['`', '(', ')', ' ', '\n', '<', '>']) {
        let piece = piece.trim_end_matches(['.', ',', ';', ':']);
        let (path, anchor) = match piece.split_once('#') {
            Some((p, a)) => (p, Some(a.to_string())),
            None => (piece, None),
        };
        if prefixes.iter().any(|p| path.starts_with(p)) && path.contains('.') {
            found.push((path.to_string(), anchor));
        }
    }
    found.sort();
    found.dedup();
    found
}

/// GitHub's heading anchor: lower case, punctuation dropped, spaces to `-`.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

fn has_heading(markdown: &str, anchor: &str) -> bool {
    markdown
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .map(|line| line.trim_start_matches('#'))
        .any(|heading| slug(heading) == anchor)
}

/// Directories `cargo package` leaves out: separate crates and excluded paths.
fn not_published(path: &str) -> bool {
    path.starts_with("integrations/")
        || path.starts_with("scripts/")
        || path.starts_with("docs/images/")
        || path.starts_with("docs/theme/")
}

#[test]
fn every_path_and_anchor_the_skill_and_llms_txt_cite_exists_in_the_crate() {
    for file in FILES {
        let text = read(file);
        let cited = cited_paths(&text);
        assert!(
            cited.len() >= 5,
            "{file}: expected many paths, got {cited:?}"
        );
        for (path, anchor) in cited {
            assert!(
                root().join(&path).exists(),
                "{file} cites a missing path: {path}"
            );
            assert!(
                !not_published(&path),
                "{file} cites {path}, which the published crate does not contain: link it as a URL"
            );
            if let Some(anchor) = anchor {
                assert!(
                    has_heading(&read(&path), &anchor),
                    "{file} cites {path}#{anchor}: no such heading"
                );
            }
        }
        // In-page anchors.
        for anchor in text
            .split("](#")
            .skip(1)
            .filter_map(|s| s.split(')').next())
        {
            assert!(
                has_heading(&text, anchor),
                "{file}: no heading for #{anchor}"
            );
        }
    }
}

/// The skill's recipe lines, without `use` lines, comments, blank lines
/// and lone braces: what must also be compiled below.
fn recipe_lines(skill: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_rust = false;
    for line in skill.lines() {
        if line.starts_with("```") {
            in_rust = line.trim() == "```rust";
            continue;
        }
        if !in_rust {
            continue;
        }
        let code = line.split(" //").next().unwrap_or_default().trim();
        if code.is_empty() || code.starts_with("use ") || ["{", "}", "})", "});"].contains(&code) {
            continue;
        }
        lines.push(code.to_string());
    }
    lines
}

#[test]
fn every_recipe_line_in_the_skill_is_compiled_here() {
    let here: Vec<String> = include_str!("skill_test.rs")
        .lines()
        .map(|l| l.split(" //").next().unwrap_or_default().trim().to_string())
        .collect();
    let recipes = recipe_lines(&read("skills/yoagent/SKILL.md"));
    assert!(
        recipes.len() >= 20,
        "found only {} recipe lines",
        recipes.len()
    );
    let missing: Vec<_> = recipes.iter().filter(|l| !here.contains(l)).collect();
    assert!(
        missing.is_empty(),
        "recipe lines in SKILL.md not compiled in tests/skill_test.rs: {missing:#?}"
    );
}

// ---- The skill's recipes, line for line. Compiled, not run. ----

#[derive(Clone)]
struct ProtectGuards;

#[async_trait::async_trait]
impl RunHooks for ProtectGuards {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let path = call.args["path"].as_str().unwrap_or_default();
        if path.starts_with(".github/") {
            ToolDecision::Deny("CI files are off limits".into())
        } else {
            ToolDecision::Allow
        }
    }
}

#[derive(Clone)]
struct RequireTests;

#[async_trait::async_trait]
impl RunHooks for RequireTests {
    async fn on_stop(&mut self, stop: &StopContext<'_>) -> StopDecision {
        let ran_tests = stop.messages.iter().any(|m| match m {
            AgentMessage::Llm(Message::Assistant { content, .. }) => content.iter().any(|c| {
                matches!(c, Content::ToolCall { name, arguments, .. }
                    if name == "bash" && arguments["command"].as_str().is_some_and(|cmd| cmd.contains("cargo test")))
            }),
            _ => false,
        });
        if ran_tests {
            StopDecision::Accept
        } else {
            StopDecision::Continue("Run `cargo test` before saying you are done.".into())
        }
    }
}

// The recipe's lines verbatim, so the `let` stays.
#[allow(dead_code, clippy::let_and_return)]
fn policy_recipe(agent: Agent) -> Agent {
    let agent = agent.with_extension(ClonedHooks::new("protect-guards", ProtectGuards).required());
    agent
}

#[allow(dead_code)]
fn budget_recipe(mut agent: Agent) -> Agent {
    yoagent::provider::prices::enable_bundled(); // pricing is opt-in since 0.25
    let config = ModelConfig::claude_sonnet_5();
    if let Some(budget) = Budget::for_model(2.00, &config) {
        // None when unpriced
        agent = agent.with_extension(budget.across_runs()); // one total for the session
    }
    agent
}

// The recipe's lines verbatim, so the `let` stays.
#[allow(dead_code, clippy::let_and_return)]
fn sub_agent_recipe(agent: Agent) -> Agent {
    let child = SubAgentTool::from_config("researcher", ModelConfig::claude_sonnet_5())
        .with_system_prompt("Research, then report.")
        .with_execution_limits(ExecutionLimits::default().with_max_turns(20));
    let agent = agent
        .with_tools(vec![Box::new(child)])
        .with_tree_extension(ClonedHooks::new("protect-guards", ProtectGuards).required());
    agent
}

/// The verifier works: it sends the agent back until a `cargo test` call is
/// in the conversation.
#[tokio::test]
async fn the_verifier_recipe_sends_the_agent_back_until_tests_ran() {
    let answer = Message::user("done");
    let without: Vec<AgentMessage> = vec![AgentMessage::Llm(Message::user("fix it"))];
    let stop = StopContext::new(&answer, &without, 0);
    assert!(matches!(
        RequireTests.on_stop(&stop).await,
        StopDecision::Continue(_)
    ));
    let call = Content::tool_call(
        "c1",
        "bash",
        serde_json::json!({"command": "cargo test --all-features"}),
    );
    let with: Vec<AgentMessage> = vec![AgentMessage::Llm(Message::assistant(
        vec![call],
        yoagent::StopReason::ToolUse,
        "m",
        "p",
        yoagent::Usage::default(),
    ))];
    let stop = StopContext::new(&answer, &with, 0);
    assert!(matches!(
        RequireTests.on_stop(&stop).await,
        StopDecision::Accept
    ));
    let _ = (Agent::from_provider(
        MockProvider::new(vec![]),
        ModelConfig::mock(),
    ),);
}

/// API names the skill mentions only in prose: a rename breaks this.
#[allow(dead_code, clippy::let_underscore_future)]
fn prose_names() {
    let _ = Agent::from_config;
    let _ = |a: Agent, b: Budget| a.with_extension(b);
    let _ = Agent::with_tools;
    let _ = |a: Agent, b: Budget| a.with_tree_extension(b);
    let _ = |a: Agent, s: std::sync::Arc<dyn yoagent::ToolSource>| a.with_tool_source(s);
    let _ = |s: &dyn yoagent::ToolSource| s.generation();
    let _ = |c: yoagent::mcp::McpClient| yoagent::mcp::McpToolSource::new(c).with_prefix("docs");
    #[cfg(feature = "native")]
    let _ = Agent::with_mcp_server_stdio;
    let _ = Agent::with_mcp_server_http;
    let _ = Agent::with_mcp_server_http_transport;
    let _ = |a: yoagent::mcp::McpToolAdapter| a.with_prefix("docs");
    async fn prefixed(client: std::sync::Arc<tokio::sync::Mutex<yoagent::mcp::McpClient>>) {
        let _ = yoagent::mcp::McpToolAdapter::from_client_with_prefix(client, "docs").await;
    }
    let _ = prefixed;
    async fn structured(a: &mut Agent) {
        let _ = a
            .prompt_structured::<serde_json::Value>("x", serde_json::json!({}))
            .await;
    }
    let _ = structured;
    let _ = Agent::steer;
    let _ = Agent::follow_up;
    let _ = Agent::abort;
    let _ = Agent::total_cost_usd;
    let _ = |p: MockProvider| Agent::from_provider(p, ModelConfig::mock());
    let _ = yoagent::retry::retry_safe_events;
    #[cfg(feature = "native")]
    let _ = yoagent::tools::default_tools;
    let _ = yoagent::session::Session::new;
    #[cfg(feature = "decision")]
    {
        use yoagent::decision::ToolGate;
        let _ = |g: ToolGate| g.with_max_argument_chars(1);
        let _ = |g: ToolGate| g.with_full_view("bash", "/command");
        let _ = ToolGate::without_default_full_views;
    }
    let _ = TurnDecision::Note;
    let _ = StopDecision::Continue;
    let _ = yoagent::ToolError::Cancelled;
    let _ = yoagent::agent_loop::CANCELLED_MARKER;
    let _ = |t: yoagent::mcp::HttpTransport| t.with_header("x-key", "v");
    let _ = |p: std::sync::Arc<dyn yoagent::provider::StreamProvider>| {
        yoagent::agent_loop::AgentLoopConfig::new(p, "model")
    };
}
