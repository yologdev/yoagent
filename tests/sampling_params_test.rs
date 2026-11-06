//! `ModelConfig::sampling_params`: a model that rejects sampling parameters
//! (a Claude model from 4.7 on) is sent no temperature (yoyo-evolve#1146).

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::mock::MockProvider;
use yoagent::provider::{ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider};
use yoagent::*;

/// Records each request's temperature.
struct Recording {
    inner: MockProvider,
    temperatures: Arc<Mutex<Vec<Option<f32>>>>,
}

#[async_trait::async_trait]
impl StreamProvider for Recording {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.temperatures.lock().unwrap().push(config.temperature);
        self.inner.stream(config, tx, cancel).await
    }
}

async fn sent_temperature(config: ModelConfig) -> Option<f32> {
    let temperatures = Arc::new(Mutex::new(Vec::new()));
    let provider = Recording {
        inner: MockProvider::texts(vec!["hi"]),
        temperatures: temperatures.clone(),
    };
    let mut agent = Agent::from_provider(provider, config).with_temperature(0.2);
    let mut rx = agent.prompt("hello").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let sent = temperatures.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    sent[0]
}

#[tokio::test]
async fn a_claude_model_from_4_7_is_sent_no_temperature() {
    assert_eq!(sent_temperature(ModelConfig::claude_opus_5_5()).await, None);
    assert_eq!(
        sent_temperature(ModelConfig::anthropic("claude-opus-4-7", "Opus 4.7")).await,
        None
    );
}

#[tokio::test]
async fn a_model_that_takes_sampling_params_is_sent_the_temperature() {
    assert_eq!(
        sent_temperature(ModelConfig::claude_haiku_4_5()).await,
        Some(0.2)
    );
    assert_eq!(sent_temperature(ModelConfig::mock()).await, Some(0.2));
}

#[tokio::test]
async fn the_flag_can_be_turned_off_for_any_model() {
    assert_eq!(
        sent_temperature(ModelConfig::mock().with_sampling_params(false)).await,
        None
    );
}

#[tokio::test]
async fn a_sub_agent_on_such_a_model_is_sent_no_temperature() {
    let temperatures = Arc::new(Mutex::new(Vec::new()));
    let child = Recording {
        inner: MockProvider::texts(vec!["done"]),
        temperatures: temperatures.clone(),
    };
    let tool = sub_agent::SubAgentTool::from_provider(
        "helper",
        Arc::new(child),
        ModelConfig::claude_opus_5_5(),
    )
    .with_temperature(0.2);
    let parent = MockProvider::new(vec![
        provider::mock::MockResponse::ToolCalls(vec![provider::mock::MockToolCall {
            name: "helper".into(),
            arguments: serde_json::json!({"task": "do it"}),
            provider_metadata: None,
        }]),
        provider::mock::MockResponse::Text("ok".into()),
    ]);
    let mut agent =
        Agent::from_provider(parent, ModelConfig::mock()).with_tools(vec![Box::new(tool)]);
    let mut rx = agent.prompt("go").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert_eq!(temperatures.lock().unwrap().clone(), vec![None]);
}

#[tokio::test]
async fn switching_to_such_a_model_mid_session_stops_the_temperature() {
    let temperatures = Arc::new(Mutex::new(Vec::new()));
    let provider = Recording {
        inner: MockProvider::texts(vec!["one", "two"]),
        temperatures: temperatures.clone(),
    };
    let mut agent =
        Agent::from_provider(provider, ModelConfig::claude_haiku_4_5()).with_temperature(0.2);
    let mut rx = agent.prompt("first").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    agent.set_model(ModelConfig::claude_opus_5_5());
    let mut rx = agent.prompt("second").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert_eq!(temperatures.lock().unwrap().clone(), vec![Some(0.2), None]);
}
