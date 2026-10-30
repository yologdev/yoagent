//! UTF-8 is framed by complete SSE events, never by HTTP body chunks.
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::{
    GoogleProvider, GoogleVertexProvider, ModelConfig, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::types::{Content, Message};

async fn fragmented_sse(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut length = 0;
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap();
                }
            }
        }
        socket.read_exact(&mut vec![0; length]).await.unwrap();
        let mut response = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec();
        // HTTP chunk framing fixes the byte boundaries without sleeps or scheduling assumptions.
        for byte in body {
            response.extend_from_slice(&[b'1', b'\r', b'\n', byte, b'\r', b'\n']);
        }
        response.extend_from_slice(b"0\r\n\r\n");
        socket.get_mut().write_all(&response).await.unwrap();
    });
    (base, server)
}

#[tokio::test]
async fn google_and_vertex_preserve_unicode_across_http_chunks() {
    for vertex in [false, true] {
        for separator in ["\n\n", "\r\n\r\n"] {
            let event = r#"{"candidates":[{"content":{"parts":[{"text":"想😀","thought":true},{"text":"你好😀"},{"functionCall":{"name":"weather","args":{"city":"東京😀"}}}],"role":"model"},"finishReason":"STOP"}]}"#;
            let (base, server) =
                fragmented_sse(format!("data: {event}{separator}").into_bytes()).await;
            let mut model = ModelConfig::google("gemini-2.5-flash", "fixture");
            model.base_url = base;
            let mut config = StreamConfig::new("gemini-2.5-flash", "fixture-key");
            config.model_config = Some(model);
            config.messages = vec![Message::user("hi")];
            let (tx, mut rx) = mpsc::unbounded_channel();
            let provider: &dyn StreamProvider = if vertex {
                &GoogleVertexProvider
            } else {
                &GoogleProvider
            };
            let message = provider
                .stream(config, tx, CancellationToken::new())
                .await
                .unwrap();
            server.await.unwrap();
            let Message::Assistant { content, .. } = message else {
                panic!("expected assistant")
            };
            assert!(
                content
                    .iter()
                    .any(|c| matches!(c, Content::Text { text } if text == "你好😀")),
                "{vertex}: {content:?}"
            );
            assert!(
                content
                    .iter()
                    .any(|c| matches!(c, Content::Thinking { thinking, .. } if thinking == "想😀")),
                "{vertex}: {content:?}"
            );
            assert!(content.iter().any(|c| matches!(c, Content::ToolCall { arguments, .. } if arguments["city"] == "東京😀")), "{vertex}: {content:?}");
            let mut text = String::new();
            let mut reasoning = String::new();
            while let Ok(event) = rx.try_recv() {
                match event {
                    StreamEvent::TextDelta { delta, .. } => text.push_str(&delta),
                    StreamEvent::ThinkingDelta { delta, .. } => reasoning.push_str(&delta),
                    _ => {}
                }
            }
            assert_eq!(text, "你好😀");
            assert_eq!(reasoning, "想😀");
        }
    }
}
