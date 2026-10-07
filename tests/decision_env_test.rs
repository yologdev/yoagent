//! Nothing is sent unless a decision model is chosen and used — even with
//! the keys and base URL in the environment.
//!
//! Its own binary because it sets process environment variables:
//! `TYPESAFE_API_KEY`, `OPENCODE_API_KEY`, `OPENAI_API_KEY` and
//! `TYPESAFE_BASE_URL`, the latter
//! pointed at a wiremock server that fails the test if it receives anything.

use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::decision::*;
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

struct Named(String);

#[async_trait::async_trait]
impl AgentTool for Named {
    fn name(&self) -> &str {
        &self.0
    }
    fn label(&self) -> &str {
        &self.0
    }
    fn description(&self) -> &str {
        "A test tool."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

/// One test, so the environment is set once and nothing races on it.
#[tokio::test]
async fn keys_in_the_environment_send_nothing_on_their_own() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    std::env::set_var("TYPESAFE_API_KEY", "sk-env-test");
    std::env::set_var("OPENCODE_API_KEY", "sk-env-test");
    std::env::set_var("TYPESAFE_BASE_URL", server.uri());
    std::env::set_var("OPENAI_API_KEY", "sk-env-test");

    // Positive control: the environment really does point `jev()` at the
    // server — a request made on purpose arrives there.
    let jev = DecisionModel::jev();
    assert!(
        format!("{jev:?}").contains(&server.uri()),
        "jev() resolves TYPESAFE_BASE_URL"
    );

    // Presets are inert: building and inspecting them sends nothing.
    for model in [
        DecisionModel::jev(),
        DecisionModel::jev_opencode(),
        DecisionModel::jev_opencode_free(),
        DecisionModel::local(server.uri()),
        DecisionModel::gpt_6_luna(),
        DecisionModel::from_openai_backend(
            OpenAiDecisionBackend::new().with_base_url(server.uri()),
            "gpt-6-luna",
        ),
    ] {
        let _ = (model.model(), model.timeout(), model.capabilities());
        let _ = ToolGate::new(model.clone());
        let _ = Advisory::new(model);
    }

    // An agent with skills and 45 tools but no decision model sends nothing.
    let tmp = tempfile::tempdir().unwrap();
    for name in ["pdf-fill", "git-release"] {
        let d = tmp.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: A skill.\n---\n\nSteps.\n"),
        )
        .unwrap();
    }
    let tools: Vec<Box<dyn AgentTool>> = (0..45)
        .map(|i| Box::new(Named(format!("tool_{i}"))) as Box<dyn AgentTool>)
        .collect();
    let mut agent = Agent::from_provider(MockProvider::text("ok"), ModelConfig::mock())
        .with_skills(yoagent::skills::SkillSet::load(&[tmp.path()]).unwrap())
        .with_tools(tools);
    let mut rx = agent.prompt("fill in the tax form PDF").await;
    let mut stats = None;
    while let Some(e) = rx.recv().await {
        if let AgentEvent::AgentEnd { stats: s, .. } = e {
            stats = Some(s);
        }
    }
    agent.finish().await;
    assert!(stats.unwrap().decision.is_empty());

    // The wiremock expectation (`expect(0)`) is verified here.
    server.verify().await;

    // Positive control for the watch itself: a deliberate request is seen.
    let e = DecisionModel::jev().noul("s", "q?").await.unwrap_err();
    assert!(
        matches!(e, DecisionError::Http { status: 500, .. }),
        "{e:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.reset().await;
}
