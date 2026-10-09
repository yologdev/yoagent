//! `PluginHost` with a real Node runtime: a plugin is reloaded after its
//! file changed (new code), kept when its new code does not load, and
//! unloaded (its tool gone).
//!
//! Needs `npm ci` in `plugins/`; skipped (passing, with `SKIPPED:`) without
//! it unless `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rutis::Ctx;
use serde_json::json;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::{Agent, AgentMessage, Content, Message};
use yoagent_frontend::host::{PluginHost, Row};
use yoagent_rutis::RutisBridge;

const WAIT: Duration = Duration::from_secs(60);

fn plugins() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins")
}

fn runtime_installed() -> bool {
    let installed = plugins()
        .join("node_modules/@arcships/rutis-runtime")
        .exists();
    if !installed {
        assert!(
            std::env::var("YOAGENT_RUTIS_REQUIRE_RUNTIMES").as_deref() != Ok("1"),
            "the plugins' packages are not installed (npm ci in {})",
            plugins().display()
        );
        println!("SKIPPED: npm ci in {} first", plugins().display());
    }
    installed
}

/// A plugin offering a `version` tool that answers `version`, and appending
/// a line to `starts` each time it starts. `@arcships/rutis` by file URL: the
/// plugin lives in a temp dir.
fn write_plugin(path: &Path, starts: &Path, version: &str) {
    write_named(path, starts, "reloadable", "version", version)
}

/// A plugin that imports fine but fails when it starts.
fn write_failing_plugin(path: &Path) {
    let rutis = plugins().join("node_modules/@arcships/rutis/src/index.mjs");
    std::fs::write(
        path,
        format!(
            r#"import {{ definePlugin }} from 'file://{}'
export default definePlugin({{
  inject: ['yoagent'],
  apply() {{
    throw new Error('this version breaks on start')
  }},
}})
"#,
            rutis.display()
        ),
    )
    .unwrap();
}

/// `write_plugin` with its own handler and tool names.
fn write_named(path: &Path, starts: &Path, handler: &str, tool: &str, version: &str) {
    let rutis = plugins().join("node_modules/@arcships/rutis/src/index.mjs");
    std::fs::write(
        path,
        format!(
            r#"import {{ appendFileSync }} from 'node:fs'
import {{ definePlugin }} from 'file://{rutis}'
export default definePlugin({{
  inject: ['yoagent'],
  apply(ctx) {{
    appendFileSync({starts:?}, '{version}\n')
    ctx.effect(ctx.use('yoagent').register('{handler}', {{
      async tools() {{
        return [{{ name: '{tool}', description: 'Which version runs.', parameters: {{ type: 'object', properties: {{}} }} }}]
      }},
      async call_tool() {{
        return {{ text: '{version}' }}
      }},
    }}))
  }},
}})
"#,
            rutis = rutis.display(),
            starts = starts.display().to_string(),
        ),
    )
    .unwrap();
}

/// What the `version` tool answers now (or the error a run gets).
async fn version(agent: &mut Agent) -> String {
    let before = agent.messages().len();
    let mut events = agent.prompt("which version?").await;
    tokio::time::timeout(WAIT, async { while events.recv().await.is_some() {} })
        .await
        .expect("the run ends");
    agent.finish().await;
    agent.messages()[before..]
        .iter()
        .find_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult { content, .. }) => {
                content.iter().find_map(|c| match c {
                    Content::Text { text } => Some(text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("the tool answered")
}

async fn registered(bridge: &RutisBridge, present: bool) {
    let registry = bridge.registry().clone();
    tokio::time::timeout(WAIT, async {
        while registry.handlers().iter().any(|h| h.name() == "reloadable") != present {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the handler (un)registered");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_reloads_with_its_edited_file_and_unloads() {
    if !runtime_installed() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // ESM, as plugin folders are: tsx loads a `.ts` file outside a
    // `"type": "module"` package as CommonJS, whose cache a reload cannot pass.
    std::fs::write(dir.path().join("package.json"), r#"{ "type": "module" }"#).unwrap();
    let file = dir.path().join("reloadable.ts");
    let starts = dir.path().join("starts.txt");
    write_plugin(&file, &starts, "version one");

    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let mut host = PluginHost::builder()
        .node("ui", plugins())
        .share("yoagent")
        .start(&root)
        .await
        .unwrap();
    host.load([Row::new("reloadable", file.display()).runtime("ui")])
        .await
        .unwrap();
    registered(&bridge, true).await;

    let call = || {
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "version".into(),
            arguments: json!({}),
            provider_metadata: None,
        }])
    };
    let done = || MockResponse::Text("ok".into());
    let mut agent = Agent::from_provider(
        MockProvider::new(vec![
            call(),
            done(),
            call(),
            done(),
            call(),
            done(),
            call(),
            done(),
            call(),
            done(),
            call(),
            done(),
            call(),
            done(),
        ]),
        ModelConfig::mock(),
    )
    .with_extension(bridge.extension());
    assert_eq!(version(&mut agent).await, "version one");

    // Edited, then reloaded: the new code runs.
    tokio::time::sleep(Duration::from_millis(20)).await;
    write_plugin(&file, &starts, "version two");
    host.reload("reloadable").await.unwrap();
    registered(&bridge, true).await;
    assert_eq!(version(&mut agent).await, "version two");

    // Reloaded without a change.
    host.reload("reloadable").await.unwrap();
    registered(&bridge, true).await;
    assert_eq!(version(&mut agent).await, "version two");
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap(),
        "version one\nversion two\nversion two\n",
        "each reload restarted it, the first with the new code"
    );

    // New code that does not load: an error, and the old code keeps running.
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(&file, "export default definePlugin({ this is not code").unwrap();
    let refused = host.reload("reloadable").await.unwrap_err().to_string();
    assert!(refused.contains("running version stays"), "{refused}");
    assert!(host.is_running("reloadable"));
    registered(&bridge, true).await;
    assert_eq!(version(&mut agent).await, "version two");

    // New code that imports but fails when it starts: the old code is gone
    // by then, so the plugin is stopped — and the error says so.
    tokio::time::sleep(Duration::from_millis(20)).await;
    write_failing_plugin(&file);
    let stopped = host.reload("reloadable").await.unwrap_err().to_string();
    assert!(stopped.contains("is stopped"), "{stopped}");
    assert!(!host.is_running("reloadable"));
    registered(&bridge, false).await;
    assert!(version(&mut agent).await.contains("not found"));
    // A stopped plugin does not block the others.
    let other = dir.path().join("other.ts");
    write_named(&other, &starts, "other", "other_version", "other one");
    host.load([Row::new("other", other.display()).runtime("ui")])
        .await
        .unwrap();
    // The next good save brings it back.
    tokio::time::sleep(Duration::from_millis(20)).await;
    write_plugin(&file, &starts, "version three");
    host.reload("reloadable").await.unwrap();
    registered(&bridge, true).await;
    assert_eq!(version(&mut agent).await, "version three");

    // Unloaded: the tool is gone.
    host.unload("reloadable").await.unwrap();
    registered(&bridge, false).await;
    let after = version(&mut agent).await;
    assert!(after.contains("not found"), "{after}");
    assert!(
        host.unload("reloadable").await.is_err(),
        "nothing left to unload"
    );
    assert!(host.reload("nope").await.is_err());
    root.shutdown().await.unwrap();
}
