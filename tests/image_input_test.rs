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
