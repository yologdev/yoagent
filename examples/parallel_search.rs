//! Anonymous Parallel Search MCP through yoagent's HTTP client and agent loop.
//!
//! Run: `cargo run --example parallel_search`
//! The MCP requests are live; the model is scripted with MockProvider, so no
//! LLM or Parallel API key is needed. This is a transport/tool-dispatch demo,
//! not an LLM-generated answer. Free-tier rate limits apply.

use std::sync::Arc;
use tokio::sync::Mutex;
use yoagent::mcp::{HttpTransport, McpClient, McpToolAdapter};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::{Agent, AgentEvent, AgentTool, Content};

const ENDPOINT: &str = "https://search.parallel.ai/mcp";
const USER_AGENT: &str = concat!(
    "yoagent/",
    env!("CARGO_PKG_VERSION"),
    " (parallel_search example)"
);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let transport = HttpTransport::new_with_user_agent(ENDPOINT, USER_AGENT)?;
    let mut client = McpClient::from_transport(Box::new(transport));
    client.initialize().await?;
    let client = Arc::new(Mutex::new(client));
    // Keep the client to explicitly close its MCP session after the run.
    let outcome = run(client.clone()).await;
    client.lock().await.close().await?;
    outcome
}

async fn run(client: Arc<Mutex<McpClient>>) -> Result<(), Box<dyn std::error::Error>> {
    let adapters = McpToolAdapter::from_client(client).await?;
    for required in ["web_search", "web_fetch"] {
        if !adapters.iter().any(|tool| tool.name() == required) {
            return Err(format!("Parallel MCP did not advertise {required}").into());
        }
    }

    // One identifier reused across the related calls for free-tier accounting.
    let session_id = uuid::Uuid::new_v4().to_string();
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "web_search".into(),
            arguments: serde_json::json!({
                "objective": "Find the official Rust book's ownership explanation.",
                "search_queries": ["Rust book ownership explanation"],
                "session_id": session_id,
            }),
            provider_metadata: None,
        }]),
        // Fetch a known official page to demonstrate the second tool. A real
        // model can instead select a URL from the preceding search results.
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "web_fetch".into(),
            arguments: serde_json::json!({
                "urls": ["https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html"],
                "objective": "Explain Rust ownership rules.",
                "search_queries": ["Rust book ownership explanation"],
                "session_id": session_id,
            }),
            provider_metadata: None,
        }]),
        MockResponse::Text("MCP tool-dispatch demo complete.".into()),
    ]);
    let tools = adapters
        .into_iter()
        .map(|tool| Box::new(tool) as Box<dyn AgentTool>)
        .collect();
    let mut agent = Agent::from_provider(provider, ModelConfig::mock()).with_tools(tools);
    let mut events = agent
        .prompt("Look up Rust ownership and fetch the official explanation.")
        .await;
    let mut completed = 0;
    let mut failure = None;
    while let Some(event) = events.recv().await {
        if let AgentEvent::ToolExecutionEnd {
            tool_name,
            result,
            is_error,
            ..
        } = event
        {
            if is_error {
                failure = Some(format!("{tool_name} failed: {:?}", result.content));
            } else {
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    failure = Some(format!("{tool_name} returned no text"));
                }
                println!("\n{tool_name}:\n{text}");
                completed += 1;
            }
        }
    }
    agent.finish().await;
    if let Some(error) = failure {
        return Err(error.into());
    }
    if completed != 2 {
        return Err(format!("Expected two successful MCP tool calls, got {completed}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    #[tokio::test]
    async fn example_discovers_and_dispatches_both_tools() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(header("User-Agent", USER_AGENT))
            .respond_with(|request: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let result = match body["method"].as_str().unwrap() {
                    "initialize" => serde_json::json!({
                        "protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                        "serverInfo": {"name": "parallel-fixture", "version": "1"}
                    }),
                    "notifications/initialized" => return ResponseTemplate::new(202),
                    "tools/list" => serde_json::json!({"tools": [
                        {"name": "web_search", "inputSchema": {"type": "object"}},
                        {"name": "web_fetch", "inputSchema": {"type": "object"}}
                    ]}),
                    "tools/call" => serde_json::json!({"content": [{
                        "type": "text", "text": "Rust ownership: each value has one owner."
                    }], "isError": false}),
                    other => panic!("Unexpected method {other}"),
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "jsonrpc": "2.0", "id": body["id"], "result": result
                }))
            })
            .expect(5)
            .mount(&server)
            .await;
        let transport =
            HttpTransport::new_with_user_agent(&format!("{}/mcp", server.uri()), USER_AGENT)
                .unwrap();
        let mut client = McpClient::from_transport(Box::new(transport));
        client.initialize().await.unwrap();
        let client = Arc::new(Mutex::new(client));
        run(client.clone()).await.unwrap();
        client.lock().await.close().await.unwrap();

        let calls: Vec<serde_json::Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .filter(|b: &serde_json::Value| b["method"] == "tools/call")
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["params"]["name"], "web_search");
        assert_eq!(calls[1]["params"]["name"], "web_fetch");
        let search = &calls[0]["params"]["arguments"];
        let fetch = &calls[1]["params"]["arguments"];
        assert_eq!(
            search["search_queries"][0],
            "Rust book ownership explanation"
        );
        assert_eq!(
            fetch["urls"][0],
            "https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html"
        );
        assert_eq!(search["session_id"], fetch["session_id"]);
        assert!(uuid::Uuid::parse_str(search["session_id"].as_str().unwrap()).is_ok());
        assert_eq!(ENDPOINT, "https://search.parallel.ai/mcp");
        server.verify().await;
    }
}
