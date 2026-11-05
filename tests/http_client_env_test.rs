//! A CA setting that loads no certificates (`SSL_CERT_FILE` naming an
//! empty file) made `reqwest::Client::new()` panic inside every provider
//! request and every decision backend's constructor, taking the agent's
//! task down (yoyo-evolve#1138). It is an error naming the variable now.
//!
//! Its own binary: it sets a process-wide variable before any client is
//! built. On Linux the TLS setup reads `SSL_CERT_FILE` (rustls-native-certs)
//! and the error is required; elsewhere the platform's store is used, and
//! only "no panic" is checked.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::{
    AnthropicProvider, ModelConfig, OpenAiCompatProvider, ProviderError, StreamConfig,
    StreamProvider,
};
use yoagent::Message;

async fn send(
    provider: &dyn StreamProvider,
    config: ModelConfig,
) -> Result<Message, ProviderError> {
    let mut stream = StreamConfig::new(config.id.clone(), "key");
    stream.messages = vec![Message::user("say ok")];
    stream.model_config = Some(config);
    let (tx, _rx) = mpsc::unbounded_channel();
    provider.stream(stream, tx, CancellationToken::new()).await
}

#[tokio::test]
async fn a_ca_setting_that_loads_nothing_is_an_error_not_a_panic() {
    let dir = tempfile::TempDir::new().unwrap();
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    // SAFETY: this binary has one test, and nothing else reads the
    // environment while it runs.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &empty);
        // A directory would add its certificates to the file's.
        std::env::remove_var("SSL_CERT_DIR");
    }
    // Nothing listens there: where the platform store is used, the request
    // fails as a network error, which is fine too.
    let base = "http://127.0.0.1:9";

    let mut anthropic = ModelConfig::anthropic("claude-x", "x");
    anthropic.base_url = base.into();
    let mut openai = ModelConfig::openai("gpt-x", "x");
    openai.base_url = base.into();
    let results = [
        send(&AnthropicProvider, anthropic).await,
        send(&OpenAiCompatProvider, openai).await,
    ];
    for result in results {
        let err = result.expect_err("nothing listens");
        if cfg!(target_os = "linux") {
            assert!(
                matches!(&err, ProviderError::Other(m) if m.contains("SSL_CERT_FILE")),
                "{err:?}"
            );
        }
    }

    #[cfg(feature = "decision")]
    {
        use yoagent::decision::{DecisionError, DecisionModel, Question};
        // Built without a panic; the failure comes back from a request.
        let model = DecisionModel::local(base);
        let err = model
            .evaluate(
                serde_json::json!({"x": 1}),
                vec![("q".into(), Question::noul("is x one?"))],
            )
            .await
            .expect_err("nothing listens");
        if cfg!(target_os = "linux") {
            assert!(
                matches!(&err, DecisionError::Backend { message, .. } if message.contains("SSL_CERT_FILE")),
                "{err:?}"
            );
            assert!(!err.is_retryable());
        }
    }
}
