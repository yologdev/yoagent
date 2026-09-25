//! `response.incomplete` mid-`arguments` on the Responses-style providers
//! (OpenAI Responses and Azure OpenAI): the turn must report
//! `StopReason::Length`, not be relabelled `ToolUse` because a tool call was
//! open, and the cut-off call must carry the unparsed marker (#167).

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::{
    unparsed_tool_arguments, AzureOpenAiProvider, ModelConfig, OpenAiResponsesProvider,
    StreamConfig, StreamProvider,
};
use yoagent::types::*;

const INCOMPLETE_MID_ARGUMENTS: &str = "event: response.function_call_arguments.start\n\
    data: {\"type\":\"response.function_call_arguments.start\",\"call_id\":\"call_1\",\"name\":\"write_file\"}\n\n\
    event: response.function_call_arguments.delta\n\
    data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"path\\\":\\\"a.txt\\\",\\\"content\\\":\\\"lo\"}\n\n\
    event: response.incomplete\n\
    data: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1,\"total_tokens\":6}}}\n\n";

const COMPLETE_CALL: &str = "event: response.function_call_arguments.start\n\
    data: {\"type\":\"response.function_call_arguments.start\",\"call_id\":\"call_1\",\"name\":\"search\"}\n\n\
    event: response.function_call_arguments.delta\n\
    data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"q\\\":\\\"x\\\"}\"}\n\n\
    event: response.completed\n\
    data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";

async fn run(provider: &dyn StreamProvider, body: &'static str) -> Message {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let mut mc = ModelConfig::openai("gpt-5.5", "GPT-5.5");
    mc.base_url = server.uri();
    let mut config = StreamConfig::new("gpt-5.5", "test-key");
    config.system_prompt = "test".into();
    config.messages = vec![Message::user("hi")];
    config.model_config = Some(mc);
    let (tx, _rx) = mpsc::unbounded_channel();
    provider
        .stream(config, tx, CancellationToken::new())
        .await
        .expect("stream should complete")
}

fn assert_length_with_marker(message: &Message) {
    let Message::Assistant {
        content,
        stop_reason,
        ..
    } = message
    else {
        panic!("expected assistant message");
    };
    assert_eq!(
        *stop_reason,
        StopReason::Length,
        "an open tool call must not overwrite the Length stop reason"
    );
    let args = content
        .iter()
        .find_map(|c| match c {
            Content::ToolCall { arguments, .. } => Some(arguments),
            _ => None,
        })
        .expect("the cut-off call still reaches the loop, which answers it");
    assert_eq!(
        unparsed_tool_arguments(args),
        Some(r#"{"path":"a.txt","content":"lo"#)
    );
}

fn stop_reason(message: &Message) -> &StopReason {
    match message {
        Message::Assistant { stop_reason, .. } => stop_reason,
        _ => panic!("expected assistant message"),
    }
}

#[tokio::test]
async fn responses_incomplete_mid_arguments_keeps_length() {
    let message = run(&OpenAiResponsesProvider, INCOMPLETE_MID_ARGUMENTS).await;
    assert_length_with_marker(&message);
}

#[tokio::test]
async fn azure_incomplete_mid_arguments_keeps_length() {
    let message = run(&AzureOpenAiProvider, INCOMPLETE_MID_ARGUMENTS).await;
    assert_length_with_marker(&message);
}

#[tokio::test]
async fn responses_complete_call_is_still_tool_use() {
    let message = run(&OpenAiResponsesProvider, COMPLETE_CALL).await;
    assert_eq!(*stop_reason(&message), StopReason::ToolUse);
}

#[tokio::test]
async fn azure_complete_call_is_still_tool_use() {
    let message = run(&AzureOpenAiProvider, COMPLETE_CALL).await;
    assert_eq!(*stop_reason(&message), StopReason::ToolUse);
}
