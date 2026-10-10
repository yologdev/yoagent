//! EXPERIMENTAL. The smallest browser frontend: one agent, one page that any
//! number of tabs share. No plugins and no Node — just a `Session`, the web
//! server and an agent with read-only tools (list, read, search), sandboxed
//! to the directory it runs in: whoever has the page's URL drives the agent.
//!
//! ```bash
//! cargo run --example minimal_browser                     # scripted model
//! DEEPSEEK_API_KEY=… cargo run --example minimal_browser -- --live
//! ```
//!
//! Open the printed URL (it carries the token the WebSocket needs). For
//! plugins, dialogs and a terminal UI, see `coding_agent`.

use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::tools::{ListFilesTool, ReadFileTool, SearchTool};
use yoagent::Agent;
use yoagent_frontend::{web, Session};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = if std::env::args().any(|arg| arg == "--live") {
        // The key from DEEPSEEK_API_KEY.
        Agent::from_config(ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"))
    } else {
        Agent::from_provider(scripted(), ModelConfig::mock())
    }
    .with_system_prompt("You are a helpful assistant. Be brief.");
    // Read-only, and only below the current directory (the built-in tools
    // allow every path unless given a sandbox).
    let here = vec![std::env::current_dir()?.display().to_string()];
    let agent = agent.with_tools(vec![
        Box::new(ListFilesTool::new().with_allowed_paths(here.clone())),
        Box::new(ReadFileTool::new().with_allowed_paths(here.clone())),
        Box::new(SearchTool::new().with_allowed_paths(here)),
    ]);

    // One agent; every browser tab that opens the URL is a frontend of it.
    let (session, driver) = Session::new(false);
    let served = web::serve(session, "127.0.0.1:8787".parse()?).await?;
    println!("open {}  (Ctrl+C to stop)", served.url());
    tokio::select! {
        _ = driver.run(agent) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}

/// Without `--live`: list the files, then answer.
fn scripted() -> MockProvider {
    MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "list_files".into(),
            arguments: serde_json::json!({ "path": "." }),
            provider_metadata: None,
        }]),
        MockResponse::Text("**Scripted** model: run with `--live` for a real one.".into()),
    ])
}
