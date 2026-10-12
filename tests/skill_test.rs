//! `skills/yoagent/SKILL.md` and `llms.txt` stay true: the skill loads
//! through yoagent's own `SkillSet`, every repository path they cite
//! exists, and the skill's recipes compile against this crate (an API
//! rename breaks this test, not the agents reading the skill).
#![cfg(feature = "native")]

use std::path::Path;

use yoagent::context::ExecutionLimits;
use yoagent::extension::{Budget, ClonedHooks, RunHooks, StopContext, StopDecision};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::{Agent, SkillSet, SubAgentTool, ToolCallRequest, ToolDecision};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

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

/// Repository paths: `docs/…`, `integrations/…`, `skills/…`, `CHANGELOG.md`,
/// in backticks or as Markdown link targets (anchors dropped).
fn cited_paths(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for piece in text.split(['`', '(', ')', ' ', '\n']) {
        let path = piece.split('#').next().unwrap_or_default();
        let repo = [
            "docs/",
            "integrations/",
            "skills/",
            "CHANGELOG.md",
            "llms.txt",
        ]
        .iter()
        .any(|p| path.starts_with(p));
        if repo && (path.ends_with(".md") || path.ends_with(".txt")) {
            found.push(path.to_string());
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn every_path_the_skill_and_llms_txt_cite_exists() {
    for file in ["skills/yoagent/SKILL.md", "llms.txt"] {
        let text = std::fs::read_to_string(root().join(file)).unwrap();
        let paths = cited_paths(&text);
        assert!(
            paths.len() >= 5,
            "{file}: expected many paths, got {paths:?}"
        );
        let missing: Vec<_> = paths.iter().filter(|p| !root().join(p).exists()).collect();
        assert!(
            missing.is_empty(),
            "{file} cites missing paths: {missing:?}"
        );
    }
}

// ---- The skill's recipes, as written there. Compiled, not run. ----

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

#[derive(Clone, Default)]
struct Verifier {
    tests_ran: bool,
}

#[async_trait::async_trait]
impl RunHooks for Verifier {
    async fn on_stop(&mut self, _stop: &StopContext<'_>) -> StopDecision {
        if self.tests_ran {
            StopDecision::Accept
        } else {
            StopDecision::Continue("Run the tests before saying you are done.".into())
        }
    }
}

#[allow(dead_code)]
fn recipes() -> Agent {
    let agent = Agent::from_provider(MockProvider::new(vec![]), ModelConfig::mock());
    let mut agent = agent
        .with_extension(ClonedHooks::new("protect-guards", ProtectGuards).required())
        .with_extension(ClonedHooks::new("verifier", Verifier::default()));

    yoagent::provider::prices::enable_bundled();
    let config = ModelConfig::claude_sonnet_5();
    if let Some(budget) = Budget::for_model(2.00, &config) {
        agent = agent.with_extension(budget.across_runs());
    }

    let child = SubAgentTool::from_config("researcher", ModelConfig::claude_sonnet_5())
        .with_system_prompt("Research, then report.")
        .with_execution_limits(ExecutionLimits::default().with_max_turns(20));
    agent
        .with_tools(vec![Box::new(child)])
        .with_tree_extension(ClonedHooks::new("protect-guards", ProtectGuards).required())
}
