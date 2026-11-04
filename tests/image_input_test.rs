//! `ModelConfig::image_input`: a text-only model is sent a note in place of
//! each image, and the history keeps the image.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::mock::MockProvider;
use yoagent::provider::{ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider};
use yoagent::*;

/// Records each request's messages.
struct Recording {
    inner: MockProvider,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait::async_trait]
impl StreamProvider for Recording {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.requests.lock().unwrap().push(config.messages.clone());
        self.inner.stream(config, tx, cancel).await
    }
}

fn image_prompt() -> AgentMessage {
    AgentMessage::Llm(Message::User {
        content: vec![
            Content::Text {
                text: "what is in it?".into(),
            },
            Content::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            },
        ],
        timestamp: 0,
    })
}

fn has_image(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::User { content, .. } | Message::ToolResult { content, .. } => {
            content.iter().any(|c| matches!(c, Content::Image { .. }))
        }
        _ => false,
    })
}

async fn run(config: ModelConfig) -> (Vec<Vec<Message>>, Agent) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Recording {
        inner: MockProvider::texts(vec!["a cat"]),
        requests: requests.clone(),
    };
    let mut agent = Agent::from_provider(provider, config);
    let mut rx = agent.prompt_messages(vec![image_prompt()]).await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let sent = requests.lock().unwrap().clone();
    (sent, agent)
}

#[tokio::test]
async fn a_model_without_image_input_is_sent_a_note_instead() {
    let (sent, agent) = run(ModelConfig::mock().with_image_input(false)).await;
    assert!(!has_image(&sent[0]));
    let Message::User { content, .. } = &sent[0][0] else {
        panic!("{:?}", sent[0][0]);
    };
    let Content::Text { text } = &content[1] else {
        panic!("{content:?}");
    };
    assert!(text.contains("takes no image input"), "{text}");
    // The history keeps the image, for a model that takes one.
    let kept: Vec<Message> = agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(m) => Some(m.clone()),
            _ => None,
        })
        .collect();
    assert!(has_image(&kept));
}

#[tokio::test]
async fn a_model_with_image_input_is_sent_the_image() {
    assert!(ModelConfig::mock().image_input, "on by default");
    let (sent, _) = run(ModelConfig::mock()).await;
    assert!(has_image(&sent[0]));
}

/// A config saved before the field existed takes images, as it did.
#[test]
fn a_persisted_config_without_the_field_takes_images() {
    let mut value = serde_json::to_value(ModelConfig::mock()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("image_input")
        .unwrap();
    let config: ModelConfig = serde_json::from_value(value).unwrap();
    assert!(config.image_input);
}

/// A tool that returns an image.
struct Screenshot;

#[async_trait::async_trait]
impl AgentTool for Screenshot {
    fn name(&self) -> &str {
        "screenshot"
    }
    fn label(&self) -> &str {
        "Screenshot"
    }
    fn description(&self) -> &str {
        "take one"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// A tool's image, the usual way one reaches a text-only model, is a note
/// too.
#[tokio::test]
async fn a_tool_image_reaches_a_text_only_model_as_a_note() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Recording {
        inner: MockProvider::new(vec![
            yoagent::provider::mock::MockResponse::ToolCalls(vec![
                yoagent::provider::mock::MockToolCall {
                    name: "screenshot".into(),
                    arguments: serde_json::json!({}),
                    provider_metadata: None,
                },
            ]),
            yoagent::provider::mock::MockResponse::Text("a login page".into()),
        ]),
        requests: requests.clone(),
    };
    let mut agent = Agent::from_provider(provider, ModelConfig::mock().with_image_input(false))
        .with_tools(vec![Box::new(Screenshot)]);
    let mut rx = agent.prompt("look").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let sent = requests.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert!(!has_image(&sent[1]));
    let Some(Message::ToolResult { content, .. }) = sent[1].last() else {
        panic!("{:?}", sent[1]);
    };
    assert!(
        matches!(&content[0], Content::Text { text } if text.contains("takes no image input")),
        "{content:?}"
    );
}
