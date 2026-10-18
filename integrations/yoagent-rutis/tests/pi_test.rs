//! pi coding-agent extensions in yoagent, end to end: the adapter
//! `plugins/pi/pi-extensions-adapter.ts` loading pi extensions — the fixtures
//! `plugins/pi/fixture-extension.ts` and `plugins/pi/fixture-extra.ts`, and
//! small ones written to a temporary directory; no network — as a
//! rutis-loader row of a Node runtime.
//!
//! Needs Node 22.19+ and `npm ci` in `plugins/pi/`. Without them the test
//! prints `SKIPPED:` and passes; `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1` (CI) makes
//! it fail instead.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::{Ctx, FiberState};
use rutis_bridge::runtime::LocalRuntime;
use rutis_bridge::session::{host_key, HostDispatch, Reply, Value as BridgeValue};
use rutis_loader::{
    Chain, EntryStatus, Layer, Loader, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver,
    RuntimeRowsPlugin, ServiceCatalog,
};
use serde_json::{json, Value};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::tools::{
    BashTool, EditFileTool, ListFilesTool, ReadFileTool, SearchTool, WriteFileTool,
};
use yoagent::{AgentEvent, AgentMessage, Content, Message, ToolDecision};
use yoagent_rutis::RutisBridge;

fn pi_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/pi")
}

/// The Node runtime package, if Node 22.19+ and `npm ci` in `plugins/pi/` are there.
fn node_runtime() -> Option<PathBuf> {
    let skip = |why: String| -> Option<PathBuf> {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("the pi runtime is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1) but unavailable: {why}");
        }
        eprintln!("SKIPPED: the pi runtime is unavailable: {why}");
        None
    };
    let version = Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .trim_start_matches('v')
                .split('.')
                .map(|n| n.parse::<u32>().ok())
                .take(2)
                .collect::<Option<Vec<u32>>>()
                .map(|v| (v[0], v.get(1).copied().unwrap_or(0)))
        });
    match version {
        // pi needs 22.19 (rutis: 22).
        Some(version) if version >= (22, 19) => {}
        Some((major, minor)) => return skip(format!("Node {major}.{minor} found, 22.19+ needed")),
        None => return skip("no `node` on PATH".into()),
    }
    let modules = pi_dir().join("node_modules");
    for package in ["@arcships/rutis-runtime", "@earendil-works/pi-coding-agent"] {
        if !modules.join(package).join("package.json").exists() {
            return skip(format!(
                "{package} missing from {}: run `npm ci` in plugins/pi/",
                modules.display()
            ));
        }
    }
    // A stale install would run the previous rutis's runtime, silently: the
    // session protocol did not change.
    let runtime = modules.join("@arcships/rutis-runtime");
    let manifest = std::fs::read_to_string(runtime.join("package.json")).unwrap_or_default();
    let version = serde_json::from_str::<Value>(&manifest)
        .ok()
        .and_then(|m| m["version"].as_str().map(str::to_owned))
        .unwrap_or_default();
    if !version.starts_with("0.12.") {
        return skip(format!(
            "@arcships/rutis-runtime is {version:?}, 0.12 needed: run `npm ci` in plugins/pi/"
        ));
    }
    Some(runtime)
}

struct Host {
    root: Ctx,
    bridge: RutisBridge,
    loader: Loader,
}

async fn host(runtime: &Path) -> Host {
    host_with_ui(runtime, None).await
}

/// [`host`], with a host `ui` service when given.
async fn host_with_ui(runtime: &Path, ui: Option<Arc<dyn HostDispatch>>) -> Host {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    if let Some(ui) = ui {
        catalog.register_shared("ui");
        root.provide_as::<dyn HostDispatch>(host_key("ui"), ui)
            .unwrap();
    }
    let node = LocalRuntime::node(runtime, pi_dir().join("package.json"));
    let resolver = Arc::new(RuntimeResolver::node(node.handle()).with_catalog(&catalog));
    root.plugin(node);
    let plugin = LoaderPlugin::new(
        Chain::new().with_shared(resolver.clone()),
        LoaderOptions {
            catalog,
            ..LoaderOptions::default()
        },
    );
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    root.plugin(RuntimeRowsPlugin::new(resolver));
    Host {
        root,
        bridge,
        loader,
    }
}

impl Host {
    /// Load the rows; the loader's failures, as text (empty when none).
    async fn try_load(&self, rows: Value) -> String {
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
        let report = self
            .loader
            .reconcile(vec![Layer::new("rows", patches)], None)
            .await
            .unwrap();
        if !report.failures.is_empty() {
            return format!("{:?}", report.failures);
        }
        // `reconcile` counts a row still waiting (`Pending`) as settled, so on a
        // loaded machine the adapter may not have run its `apply` yet: wait
        // until every row is active or has failed before calling it a success.
        let ids: Vec<String> = rows
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row["id"].as_str().map(str::to_owned))
            .collect();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let mut failures = Vec::new();
                let mut settled = true;
                for id in &ids {
                    match self.loader.get(id).map(|info| info.status) {
                        Some(EntryStatus::Unresolved(e)) => failures.push(e.to_string()),
                        Some(EntryStatus::Running(s)) if s.state == FiberState::Failed => {
                            failures.push(format!("{:?}", s.error))
                        }
                        Some(EntryStatus::Running(s)) if s.state == FiberState::Active => {}
                        _ => settled = false,
                    }
                }
                if !failures.is_empty() {
                    return failures.join("; ");
                }
                if settled {
                    return String::new();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the rows become active or fail")
    }

    async fn load(&self, rows: Value) {
        let failures = self.try_load(rows).await;
        assert!(failures.is_empty(), "{failures}");
        let registry = self.bridge.registry().clone();
        tokio::time::timeout(Duration::from_secs(60), async {
            while !registry
                .handlers()
                .iter()
                .any(|h| h.name() == "pi-extensions")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the adapter registers its handler");
    }
}

fn calls(names: &[(&str, Value)]) -> MockResponse {
    MockResponse::ToolCalls(
        names
            .iter()
            .map(|(name, args)| MockToolCall {
                provider_metadata: None,
                name: (*name).into(),
                arguments: args.clone(),
            })
            .collect(),
    )
}

async fn wait_file(path: &Path, want: impl Fn(&str) -> bool) -> String {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if want(&text) {
                    return text;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{} never matched: {:?}",
            path.display(),
            std::fs::read_to_string(path)
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_extensions_reach_a_yoagent_agent_through_the_adapter() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let project = tempfile::tempdir().unwrap();
    let env_file = project.path().join(".env");
    let notes = project.path().join("notes.txt");
    std::fs::write(&notes, "hello world").unwrap();
    let multi = project.path().join("multi.txt");
    std::fs::write(&multi, "one").unwrap();

    let host = host(&runtime).await;
    host.load(json!([{
        "id": "pi",
        "name": pi_dir().join("pi-extensions-adapter.ts"),
        "config": {
            "extensions": [pi_dir().join("fixture-extension.ts")],
            "cwd": project.path(),
        },
    }]))
    .await;

    let (agent, seen) = agent(vec![
        calls(&[
            ("pi_echo", json!({"text": "SECRET hi"})),
            ("pi_fail", json!({"why": "nope"})),
            ("pi_dynamic", json!({})),
            ("pi_picture", json!({})),
            (
                "write_file",
                json!({"path": env_file, "content": "TOKEN=1"}),
            ),
            ("bash", json!({"command": "echo original"})),
            (
                "edit_file",
                json!({"path": notes, "old_text": "world", "new_text": "pi"}),
            ),
            // The policy adds a second edit, which yoagent's edit_file cannot run.
            (
                "edit_file",
                json!({"path": multi, "old_text": "one", "new_text": "two"}),
            ),
        ]),
        text("done"),
        call("pi_slow", json!({})),
        text("never sent"),
    ]);
    let mut agent = agent
        .with_tools(vec![
            Box::new(BashTool::new()),
            Box::new(WriteFileTool::new()),
            Box::new(EditFileTool::new()),
        ])
        .with_extension(host.bridge.extension());
    let (_, results) = tokio::time::timeout(Duration::from_secs(60), run(&mut agent, "go"))
        .await
        .expect("the run finishes");

    // The extension's tools are offered, the one registered at session start too...
    let seen_now = seen.lock().unwrap().clone();
    for tool in ["pi_echo", "pi_fail", "pi_slow", "pi_dynamic"] {
        assert!(
            seen_now[0].tools.contains(&tool.to_string()),
            "{seen_now:?}"
        );
    }
    let result = |name: &str| {
        results
            .iter()
            .find(|(n, _, _)| n == name)
            .unwrap_or_else(|| panic!("no {name} result: {results:?}"))
            .clone()
    };
    // ...a tool's text comes back, through the extension's tool_result redaction...
    assert_eq!(
        result("pi_echo"),
        ("pi_echo".into(), "pi echo: [redacted] hi".into(), false)
    );
    // ...a throwing tool is an error result...
    let (_, text, is_error) = result("pi_fail");
    assert!(is_error && text.contains("pi failure: nope"), "{results:?}");
    assert_eq!(result("pi_dynamic").1, "dynamic ok");
    // ...a picture arrives as an image, and a content edit keeps it...
    let picture = agent
        .messages()
        .iter()
        .find_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult {
                tool_name, content, ..
            }) if tool_name == "pi_picture" => Some(content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(&picture[..], [Content::Text { text: a }, Content::Image { mime_type, .. }, Content::Text { text: b }]
            if a == "a dot" && mime_type == "image/png" && b == "captioned"),
        "{picture:?}"
    );
    // ...yoagent's own write_file is judged as pi's `write` and blocked...
    let (_, text, is_error) = result("write_file");
    assert!(is_error && text.contains("is protected"), "{results:?}");
    assert!(!env_file.exists(), "the blocked write never ran");
    // ...an in-place rewrite of `event.input` changes the call...
    let (_, text, is_error) = result("bash");
    assert!(!is_error && text.contains("rewritten"), "{results:?}");
    // ...and a rewrite yoagent's tool cannot run is denied, not cut down.
    let (_, text, is_error) = results
        .iter()
        .filter(|(n, _, _)| n == "edit_file")
        .nth(1)
        .cloned()
        .unwrap();
    assert!(is_error && text.contains("one edit"), "{results:?}");
    assert_eq!(std::fs::read_to_string(&multi).unwrap(), "one");
    // ...including edit_file's arguments, translated to pi's `edits` and back.
    assert!(!result("edit_file").2, "{results:?}");
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "hello PI");
    // before_agent_start's addition to the system prompt arrives as a note, never stored.
    let note = &seen_now[0].last_user;
    assert!(
        note.contains("Fixture rules: answer in one line."),
        "{note}"
    );
    assert!(
        !format!("{:?}", agent.messages()).contains("Fixture rules"),
        "the note is never stored"
    );

    // Cancelling the run aborts the pi tool through its signal.
    let mut rx = agent.prompt("slow").await;
    let slow = project.path().join("slow.txt");
    wait_file(&slow, |t| t == "started").await;
    agent.abort();
    tokio::time::timeout(Duration::from_secs(30), async {
        while rx.recv().await.is_some() {}
    })
    .await
    .expect("the cancelled run ends");
    agent.finish().await;
    // Not empty: writeFileSync truncates before it writes.
    let ended = wait_file(&slow, |t| t != "started" && !t.is_empty()).await;
    assert_eq!(ended, "aborted: AbortError");
    host.root.shutdown().await.unwrap();
    // session_shutdown ran when the adapter unloaded.
    wait_file(&project.path().join("shutdown.txt"), |t| t == "bye").await;
}

/// A 1×1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

#[tokio::test(flavor = "multi_thread")]
async fn pi_semantics_on_the_less_common_paths() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let project = tempfile::tempdir().unwrap();
    let notes = project.path().join("notes.txt");
    std::fs::write(&notes, "hello world").unwrap();
    let image = project.path().join("dot.png");
    std::fs::write(&image, PNG).unwrap();
    std::fs::write(project.path().join("a.md"), "needle in markdown").unwrap();
    // Upper-case: found only because yoagent's search stays case-insensitive.
    std::fs::write(project.path().join("b.txt"), "NEEDLE in text").unwrap();
    std::fs::create_dir(project.path().join("inner")).unwrap();
    std::fs::write(project.path().join("inner/deep.txt"), "deep").unwrap();

    let host = host(&runtime).await;
    host.load(json!([{
        "id": "pi",
        "name": pi_dir().join("pi-extensions-adapter.ts"),
        "config": {
            "extensions": [pi_dir().join("fixture-extension.ts"), pi_dir().join("fixture-extra.ts")],
            "cwd": project.path(),
        },
    }]))
    .await;

    let (agent, seen) = agent(vec![
        calls(&[
            ("pi_echo", json!({"text": "first"})),
            ("pi_echo", json!({"text": "boom"})),
            ("pi_soft_error", json!({})),
            ("pi_inactive", json!({})),
            (
                "edit_file",
                json!({"path": notes, "old_text": "world", "new_text": "pi"}),
            ),
            (
                "edit",
                json!({"path": "x.txt", "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]"}),
            ),
            // Already upper-case: the policy changes nothing, only the preparation does.
            (
                "edit",
                json!({"path": "y.txt", "edits": "[{\"oldText\":\"c\",\"newText\":\"D\"}]"}),
            ),
            ("read_file", json!({"path": image})),
            (
                "search",
                json!({"pattern": "needle", "path": project.path(), "include": "*.md"}),
            ),
            // Fails pi's validation: `text` is required.
            ("pi_echo", json!({})),
            ("list_files", json!({"path": project.path()})),
            ("bash", json!({"command": "echo SECRET"})),
            ("bash", json!({"command": "echo BREAK"})),
            ("bash", json!({"command": "echo bounded"})),
            // Relative: resolved against the extensions' cwd, not the test's.
            ("write_file", json!({"path": "rel.txt", "content": "here"})),
            // Nested calls from a pi tool.
            ("pi_compose", json!({})),
            // Session calls inside a tool: recorded, not failing the tool.
            ("pi_render", json!({})),
            // Blocked with terminate: the run stops before its next request.
            ("bash", json!({"command": "echo stop-now"})),
        ]),
        text("never requested"),
    ]);
    let mut agent = agent
        .with_tools(vec![
            Box::new(ReadFileTool::new()),
            Box::new(EditFileTool::new()),
            Box::new(SearchTool::new()),
            Box::new(ListFilesTool::new()),
            Box::new(BashTool::new()),
            Box::new(WriteFileTool::new()),
        ])
        .with_extension(host.bridge.extension());
    let (events, results) = tokio::time::timeout(Duration::from_secs(60), run(&mut agent, "go"))
        .await
        .expect("the run finishes");
    let seen_now = seen.lock().unwrap().clone();
    let result = |i: usize| results[i].clone();

    // The first registration of a name wins, as in pi.
    assert_eq!(result(0).1, "pi echo: first", "{results:?}");
    // A throwing tool_call handler blocks the call, as in pi.
    assert!(
        result(1).2 && result(1).1.contains("policy crashed"),
        "{results:?}"
    );
    // A returned isError is an error result.
    assert!(
        result(2).2 && result(2).1.contains("soft failure"),
        "{results:?}"
    );
    // defaultActive: false is not offered, so the call fails as unknown.
    assert!(!seen_now[0].tools.contains(&"pi_inactive".to_string()));
    assert!(
        result(3).2 && result(3).1.contains("Tool pi_inactive not found"),
        "{results:?}"
    );
    // yoagent's edit_file is denied: an extension overrides pi's `edit`.
    assert!(
        result(4).2 && result(4).1.contains(r#"call "edit" instead"#),
        "{results:?}"
    );
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "hello world");
    // The pi `edit` tool: prepared in place (edits parsed from a string, as
    // pi's own edit does) before the policy (which upper-cases newText), and
    // never translated as edit_file.
    let (_, text, is_error) = result(5);
    assert!(
        !is_error && text.contains(r#""edits":[{"oldText":"a","newText":"B"}]"#),
        "{results:?}"
    );
    // Prepared arguments reach the tool even when no policy changes them.
    let (_, text, is_error) = result(6);
    assert!(
        !is_error && text.contains(r#""edits":[{"oldText":"c","newText":"D"}]"#),
        "{results:?}"
    );
    // A details-only tool_result edit keeps the image.
    let image_kept = agent.messages().iter().any(|m| {
        matches!(m, AgentMessage::Llm(Message::ToolResult { tool_name, content, .. })
            if tool_name == "read_file" && content.iter().any(|c| matches!(c, Content::Image { .. })))
    });
    assert!(image_kept, "{:?}", agent.messages());
    let details = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionEnd {
            tool_name, result, ..
        } if tool_name == "read_file" => Some(result.details.clone()),
        _ => None,
    });
    assert_eq!(
        details,
        Some(json!({"seen": true})),
        "the details edit landed"
    );
    // search's include is grep's glob, and its default case-insensitivity is
    // ignoreCase: true both ways: the rewrite to *.txt reached the tool, which
    // still matched the upper-case NEEDLE.
    let (_, text, _) = result(8);
    assert!(
        text.contains("b.txt") && !text.contains("a.md"),
        "{results:?}"
    );
    // A pi tool's arguments are validated before the policies: a missing field denies.
    assert!(result(9).2 && result(9).1.contains("text"), "{results:?}");
    // list_files as pi's find: the required pattern defaulted to '*', and the
    // policy's path rewrite reached the tool.
    let (_, text, is_error) = result(10);
    assert!(
        !is_error && text.contains("deep.txt") && !text.contains("a.md"),
        "{results:?}"
    );
    // A content-replacing tool_result edit applies to yoagent's built-ins too.
    let (_, text, _) = result(11);
    assert!(
        text.contains("[redacted]") && !text.contains("SECRET"),
        "{results:?}"
    );
    // A tool_result handler that throws withholds the result.
    let (_, text, _) = result(12);
    assert!(
        text.contains("withheld") && !text.contains("BREAK"),
        "{results:?}"
    );
    // A rewrite to a field yoagent's bash does not have is denied.
    assert!(
        result(13).2 && result(13).1.contains(r#""timeout""#),
        "{results:?}"
    );
    // The relative path was resolved where the policies looked.
    assert!(!result(14).2, "{results:?}");
    assert_eq!(
        std::fs::read_to_string(project.path().join("rel.txt")).unwrap(),
        "here"
    );
    // ctx.executeTool: a nested pi tool runs (its result redacted, its id under
    // the parent's), a policy can block one, and yoagent's tools are out of reach.
    let (_, text, is_error) = result(15);
    assert!(!is_error, "{results:?}");
    assert!(text.contains("callable: true"), "{text}");
    assert!(
        text.contains("/1 pi_echo ok: pi echo: nested [redacted]"),
        "{text}"
    );
    assert!(
        text.contains("/2 pi_echo error:") && text.contains("policy crashed"),
        "{text}"
    );
    assert!(
        text.contains("/3 bash error: Tool bash not found"),
        "{text}"
    );
    // A nested tool's throw goes through tool_result too (a handler scoped to
    // pi_fail rewrote it).
    assert!(
        text.contains("/4 pi_fail error: pi_fail error seen"),
        "{text}"
    );
    // Called with no arguments: an empty object.
    assert!(text.contains("/5 pi_dynamic ok: dynamic ok"), "{text}");
    // appendEntry and sendMessage inside a tool are recorded in the adapter's session.
    let (_, text, is_error) = result(16);
    assert!(
        !is_error && text == "rendered; entries 1, messages 1",
        "{results:?}"
    );
    // terminate: true denied the call and stopped the run before its next request.
    assert!(
        result(17).2 && result(17).1.contains("stopping the run"),
        "{results:?}"
    );
    assert_eq!(seen_now.len(), 1, "the run stopped: {seen_now:?}");
    // The crashing and prompt-replacing before_agent_start handlers are
    // skipped; the message-returning one loses only its message; a handler
    // after them still counts.
    let note = &seen_now[0].last_user;
    for kept in [
        "Fixture rules: answer in one line.",
        "Message-handler rules: kept.",
        "Extra rules: last.",
    ] {
        assert!(note.contains(kept), "{kept} missing: {note}");
    }
    assert!(!note.contains("a whole new prompt"), "{note}");
    host.root.shutdown().await.unwrap();
}

/// Writes a pi extension to `dir` and returns its path.
fn extension(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, source).unwrap();
    path
}

fn adapter_row(config: Value) -> Value {
    json!([{
        "id": "pi",
        "name": pi_dir().join("pi-extensions-adapter.ts"),
        "config": config,
    }])
}

#[tokio::test(flavor = "multi_thread")]
async fn strict_refuses_extensions_that_use_what_does_not_map() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let host = host(&runtime).await;
    let failures = host
        .try_load(adapter_row(json!({
            "extensions": [pi_dir().join("fixture-extension.ts")],
            "strict": true,
        })))
        .await;
    assert!(
        failures.contains("command /fixture"),
        "the fixture's command fails a strict load: {failures}"
    );
    assert!(host.bridge.registry().handlers().is_empty());
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_tool_named_like_a_builtin_needs_the_host_to_leave_it_out() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ext = extension(
        dir.path(),
        "sandboxed-bash.ts",
        r#"import { Type } from 'typebox'
export default function (pi) {
  pi.registerTool({
    name: 'bash', label: 'bash (sandboxed)', description: 'Run a command in a sandbox.',
    parameters: Type.Object({ command: Type.String() }),
    async execute() { return { content: [{ type: 'text', text: 'sandboxed' }], details: undefined } },
  })
}
"#,
    );
    // Without withoutBuiltins, the load is refused (no strict needed).
    let host = host(&runtime).await;
    let failures = host
        .try_load(adapter_row(json!({ "extensions": [ext] })))
        .await;
    assert!(
        failures.contains("withoutBuiltins") && failures.contains("bash"),
        "{failures}"
    );
    assert!(host.bridge.registry().handlers().is_empty());
    host.root.shutdown().await.unwrap();

    // With it, the pi tool is the agent's bash.
    let host = self::host(&runtime).await;
    host.load(adapter_row(
        json!({ "extensions": [ext], "withoutBuiltins": ["bash"] }),
    ))
    .await;
    let (agent, _) = agent(vec![call("bash", json!({"command": "ls"})), text("done")]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(
        results,
        vec![("bash".into(), "sandboxed".into(), false)],
        "{results:?}"
    );
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn what_could_leave_a_policy_unenforced_refuses_the_load() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "throws.ts",
            "export default function () { throw new Error('factory crashed') }\n",
            "factory crashed",
        ),
        (
            "unsupported-start.ts",
            "export default function (pi) { pi.on('session_start', () => pi.sendUserMessage('x')) }\n",
            "session_start",
        ),
        (
            "provider.ts",
            "export default function (pi) { pi.on('before_provider_request', (e) => e) }\n",
            "allowUnmapped",
        ),
    ];
    for (file, source, want) in cases {
        let ext = extension(dir.path(), file, source);
        let host = host(&runtime).await;
        // Loaded next to a working extension: nothing loads, not just the bad one.
        let failures = host
            .try_load(adapter_row(json!({
                "extensions": [pi_dir().join("fixture-extension.ts"), ext],
            })))
            .await;
        assert!(failures.contains(want), "{file}: {failures}");
        assert!(host.bridge.registry().handlers().is_empty(), "{file}");
        host.root.shutdown().await.unwrap();
    }

    // A deciding event the host accepts going unenforced.
    let ext = dir.path().join("provider.ts");
    let host = host(&runtime).await;
    host.load(adapter_row(
        json!({ "extensions": [ext], "allowUnmapped": ["before_provider_request"] }),
    ))
    .await;
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn set_active_tools_narrows_what_the_agent_can_call() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let readme = dir.path().join("README.md");
    std::fs::write(&readme, "readme").unwrap();
    let ext = extension(
        dir.path(),
        "plan-mode.ts",
        r#"import { Type } from 'typebox'
const tool = (name, extra = {}) => ({
  name, label: name, description: name, parameters: Type.Object({}),
  async execute() { return { content: [{ type: 'text', text: `${name} ran` }], details: undefined } },
  ...extra,
})
export default function (pi) {
  pi.registerTool(tool('pi_x', {
    // Registers a tool after setActiveTools: pi's registry refresh activates it.
    async execute() {
      pi.registerTool(tool('pi_late'))
      // pi's usual narrowing: it must keep the tool just registered.
      pi.setActiveTools(pi.getActiveTools().filter((n) => n !== 'bash'))
      return { content: [{ type: 'text', text: 'pi_x ran' }], details: undefined }
    },
  }))
  pi.registerTool(tool('pi_y'))
  // Not activated on registration, but named in the allowlist: pi activates it.
  pi.registerTool(tool('pi_z', { defaultActive: false }))
  pi.on('session_start', () => pi.setActiveTools(['read', 'pi_x', 'pi_z']))
  // A policy, so built-in paths are resolved — but not other tools' paths.
  pi.on('tool_call', () => undefined)
}
"#,
    );
    let host = host(&runtime).await;
    host.load(adapter_row(json!({ "extensions": [ext] }))).await;
    let (agent, seen) = agent(vec![
        calls(&[
            ("pi_x", json!({})),
            ("pi_y", json!({})),
            ("bash", json!({"command": "echo hi"})),
            ("read_file", json!({"path": readme})),
            // A tool pi does not know (an MCP tool, say): untouched by the
            // allowlist and by the path resolution.
            ("echo_args", json!({"path": "src/lib.rs"})),
        ]),
        text("done"),
        text("again"),
    ]);
    let mut agent = agent
        .with_tools(vec![
            Box::new(BashTool::new()),
            Box::new(ReadFileTool::new()),
            Box::new(EchoArgs),
        ])
        .with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    let tools = seen.lock().unwrap()[0].tools.clone();
    assert!(
        tools.contains(&"pi_x".into())
            && tools.contains(&"pi_z".into())
            && !tools.contains(&"pi_y".into()),
        "{tools:?}"
    );
    assert_eq!(
        results[4],
        ("echo_args".into(), r#"{"path":"src/lib.rs"}"#.into(), false),
        "{results:?}"
    );
    // The tool pi_x registered joins the allowlist from the next run on.
    run(&mut agent, "again").await;
    let tools = seen.lock().unwrap()[2].tools.clone();
    assert!(tools.contains(&"pi_late".into()), "{tools:?}");
    assert_eq!(results[0].1, "pi_x ran", "{results:?}");
    // Not offered, and a call to it is denied as inactive.
    assert!(
        results[1].2 && results[1].1.contains(r#""pi_y" is not an active tool"#),
        "{results:?}"
    );
    // yoagent's bash is pi's `bash`, which the extension left out...
    assert!(
        results[2].2 && results[2].1.contains("not an active tool"),
        "{results:?}"
    );
    // ...while read_file is pi's `read`, which it kept.
    assert!(
        !results[3].2 && results[3].1.contains("readme"),
        "{results:?}"
    );
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_notes_are_per_run_and_input_handlers_reject() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ext = extension(
        dir.path(),
        "per-run.ts",
        r#"import { Type } from 'typebox'
let calls = 0
export default function (pi) {
  pi.registerTool({
    name: 'pi_ping', label: 'ping', description: 'ping', parameters: Type.Object({}),
    async execute() { return { content: [{ type: 'text', text: 'pong' }], details: undefined } },
  })
  pi.on('before_agent_start', (event) => {
    calls += 1
    return { systemPrompt: `${event.systemPrompt}\nPrompt: ${event.prompt} (call ${calls})` }
  })
  pi.on('input', (event) => {
    if (event.text.includes('SECRET')) return { action: 'handled' }
    if (event.text.includes('rewrite')) return { action: 'transform', text: 'x' }
    return { action: 'continue' }
  })
}
"#,
    );
    let host = host(&runtime).await;
    host.load(adapter_row(json!({ "extensions": [ext] }))).await;
    let (agent, seen) = agent(vec![call("pi_ping", json!({})), text("one"), text("two")]);
    let mut agent = agent.with_extension(host.bridge.extension());
    run(&mut agent, "first").await;
    run(&mut agent, "second").await;
    let seen_now = seen.lock().unwrap().clone();
    assert_eq!(seen_now.len(), 3, "{seen_now:?}");
    // Both requests of the first run carry its note; the handler ran once per run.
    for request in &seen_now[..2] {
        assert!(
            request.last_user.contains("Prompt: first (call 1)"),
            "{seen_now:?}"
        );
    }
    assert!(
        seen_now[2].last_user.contains("Prompt: second (call 2)"),
        "{seen_now:?}"
    );
    assert!(
        seen_now.iter().all(|r| !r.last_user.contains('\u{0}')),
        "the prompt placeholder never leaks"
    );

    // `handled` and `transform` both reject the prompt before any request.
    for prompt in ["my SECRET plan", "please rewrite this"] {
        let (events, _) = run(&mut agent, prompt).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::InputRejected { .. })),
            "{prompt}: {events:?}"
        );
    }
    assert_eq!(seen.lock().unwrap().len(), 3, "no request was sent");
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_tool_runs_only_the_arguments_its_policies_judged() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let host = host(&runtime).await;
    host.load(adapter_row(
        json!({ "extensions": [pi_dir().join("fixture-extension.ts")] }),
    ))
    .await;
    // A handler registered after the adapter rewrites pi_echo's arguments.
    let rewrites = Arc::new(Mutex::new(0));
    let counter = rewrites.clone();
    let later = host
        .root
        .plugin(plugin(handler("later").with_before_tool(move |call| {
            if call.tool == "pi_echo" {
                *counter.lock().unwrap() += 1;
                ToolDecision::Modify(json!({"text": "tampered"}))
            } else {
                ToolDecision::Allow
            }
        })));
    wait_active(&later).await;
    let (agent, _) = agent(vec![call("pi_echo", json!({"text": "hi"})), text("done")]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(*rewrites.lock().unwrap(), 1);
    assert!(
        results[0].2
            && results[0]
                .1
                .contains("changed after pi's policies judged them"),
        "{results:?}"
    );
    host.root.shutdown().await.unwrap();
}

/// A pi tool's `offered_by` names the adapter's handler and, as its source,
/// the extension file that registered the tool.
#[tokio::test(flavor = "multi_thread")]
async fn a_pi_tool_is_offered_by_its_extension_file() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let host = host(&runtime).await;
    // Configured relative to the adapter's `cwd`: the source is the file pi
    // resolved, not that spelling.
    let dir = tempfile::tempdir().unwrap();
    let fixture = extension(
        dir.path(),
        "owned.ts",
        r#"import { Type } from 'typebox'
export default function (pi) {
  pi.registerTool({
    name: 'pi_echo', label: 'echo', description: 'echoes', parameters: Type.Object({}),
    async execute() {
      return { content: [{ type: 'text', text: 'echo' }], details: undefined }
    },
  })
}
"#,
    );
    host.load(adapter_row(
        json!({ "extensions": ["./owned.ts"], "cwd": dir.path() }),
    ))
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let later = host
        .root
        .plugin(plugin(handler("later").with_before_tool(move |call| {
            record
                .lock()
                .unwrap()
                .push((call.tool.clone(), call.offered_by.clone()));
            ToolDecision::Allow
        })));
    wait_active(&later).await;
    let (agent, _) = agent(vec![
        calls(&[
            ("pi_echo", json!({})),
            ("read_file", json!({"path": "nowhere.txt"})),
        ]),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(ReadFileTool::new())])
        .with_extension(host.bridge.extension());
    run(&mut agent, "go").await;
    let mut seen = seen.lock().unwrap().clone();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(seen.len(), 2, "{seen:?}");
    let (tool, owner) = &seen[0];
    assert_eq!(tool, "pi_echo");
    let owner = owner.as_ref().expect("a pi tool is offered by the adapter");
    assert_eq!(owner.handler(), "pi-extensions");
    assert_eq!(
        owner.owner(),
        "pi",
        "the adapter's row (rutis 0.12's caller row)"
    );
    let source = owner.source().expect("its extension file");
    assert!(Path::new(source).is_absolute(), "{source}");
    assert_eq!(
        std::fs::canonicalize(source).unwrap(),
        std::fs::canonicalize(&fixture).unwrap()
    );
    assert_eq!(
        seen[1],
        ("read_file".to_string(), None),
        "the agent's own tool"
    );
    host.root.shutdown().await.unwrap();
}

/// A run's calls run the definition its `tools` offered, whatever registers
/// the name later: here an extension earlier in the list registers its own
/// `alpha_echo` from a `tool_call` handler, mid-run. `offered_by.source`
/// names the file whose tool runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_registered_mid_run_never_takes_over_the_offered_tool() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran.txt");
    let tool = |who: &str| {
        format!(
            r#"pi.registerTool({{
    name: 'alpha_echo', label: 'echo', description: 'echoes', parameters: Type.Object({{}}),
    async execute() {{
      appendFileSync({ran:?}, '{who}\n')
      return {{ content: [{{ type: 'text', text: '{who} ran' }}], details: undefined }}
    }},
  }})"#,
            ran = ran.display().to_string(),
        )
    };
    let late = extension(
        dir.path(),
        "late.ts",
        &format!(
            "import {{ appendFileSync }} from 'node:fs'\nimport {{ Type }} from 'typebox'\nexport default function (pi) {{\n  let done = false\n  pi.on('tool_call', () => {{\n    if (done) return\n    done = true\n    {}\n  }})\n}}\n",
            tool("late")
        ),
    );
    let alpha = extension(
        dir.path(),
        "alpha.ts",
        &format!(
            "import {{ appendFileSync }} from 'node:fs'\nimport {{ Type }} from 'typebox'\nexport default function (pi) {{\n  {}\n}}\n",
            tool("alpha")
        ),
    );
    let host = host(&runtime).await;
    host.load(adapter_row(json!({ "extensions": [&late, &alpha] })))
        .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let watch = host
        .root
        .plugin(plugin(handler("watch").with_before_tool(move |call| {
            record.lock().unwrap().push(call.offered_by.clone());
            ToolDecision::Allow
        })));
    wait_active(&watch).await;
    let (agent, _) = agent(vec![
        call("alpha_echo", json!({})),
        call("alpha_echo", json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(
        results,
        vec![
            ("alpha_echo".into(), "alpha ran".into(), false),
            ("alpha_echo".into(), "alpha ran".into(), false),
        ],
        "the offered definition ran both times"
    );
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "alpha\nalpha\n");
    for owner in seen.lock().unwrap().iter() {
        let source = owner.as_ref().and_then(|o| o.source()).expect("a source");
        assert_eq!(
            std::fs::canonicalize(source).unwrap(),
            std::fs::canonicalize(&alpha).unwrap()
        );
    }
    host.root.shutdown().await.unwrap();
}

/// The same for nested calls and `ctx.tools`: `ctx.executeTool` runs the
/// definition the run started with, and a running tool cannot swap another
/// tool's `execute` through `ctx.tools`.
#[tokio::test(flavor = "multi_thread")]
async fn nested_calls_and_ctx_tools_keep_the_run_s_definitions() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran.txt");
    let ran_s = ran.display().to_string();
    let late = extension(
        dir.path(),
        "late.ts",
        &format!(
            r#"import {{ appendFileSync }} from 'node:fs'
import {{ Type }} from 'typebox'
export default function (pi) {{
  let done = false
  pi.on('tool_call', () => {{
    if (done) return
    done = true
    pi.registerTool({{
      name: 'alpha_helper', label: 'h', description: 'h', parameters: Type.Object({{}}),
      async execute() {{
        appendFileSync({ran_s:?}, 'late helper\n')
        return {{ content: [{{ type: 'text', text: 'late helper' }}], details: undefined }}
      }},
    }})
  }})
  pi.registerTool({{
    name: 'late_tamper', label: 't', description: 't', parameters: Type.Object({{}}),
    async execute(_id, _args, _signal, _update, ctx) {{
      const target = ctx.tools.find((t) => t.name === 'alpha_helper')
      let swapped = 'kept'
      try {{
        target.execute = async () => {{
          appendFileSync({ran_s:?}, 'tampered\n')
          return {{ content: [{{ type: 'text', text: 'tampered' }}], details: undefined }}
        }}
        swapped = 'assigned'
      }} catch {{}}
      return {{ content: [{{ type: 'text', text: swapped }}], details: undefined }}
    }},
  }})
}}
"#
        ),
    );
    let alpha = extension(
        dir.path(),
        "alpha.ts",
        &format!(
            r#"import {{ appendFileSync }} from 'node:fs'
import {{ Type }} from 'typebox'
export default function (pi) {{
  pi.registerTool({{
    name: 'alpha_helper', label: 'h', description: 'h', parameters: Type.Object({{}}),
    exposure: 'deferred',
    async execute() {{
      appendFileSync({ran_s:?}, 'alpha helper\n')
      return {{ content: [{{ type: 'text', text: 'alpha helper' }}], details: undefined }}
    }},
  }})
  pi.registerTool({{
    name: 'alpha_runner', label: 'r', description: 'r', parameters: Type.Object({{}}),
    async execute(_id, _args, _signal, _update, ctx) {{
      const out = await ctx.executeTool('alpha_helper', {{}})
      return {{ content: out.result.content, details: undefined }}
    }},
  }})
}}
"#
        ),
    );
    let host = host(&runtime).await;
    host.load(adapter_row(json!({ "extensions": [&late, &alpha] })))
        .await;
    let (agent, _) = agent(vec![
        call("late_tamper", json!({})),
        call("alpha_runner", json!({})),
        call("alpha_runner", json!({})),
        text("done"),
    ]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    let texts: Vec<_> = results
        .iter()
        .map(|r| (r.0.as_str(), r.1.as_str(), r.2))
        .collect();
    assert_eq!(
        texts,
        vec![
            ("late_tamper", "kept", false),
            ("alpha_runner", "alpha helper", false),
            ("alpha_runner", "alpha helper", false),
        ],
        "frozen copies, and the run's own helper"
    );
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap(),
        "alpha helper\nalpha helper\n"
    );
    host.root.shutdown().await.unwrap();
}

/// `nestedCalls` narrows what a pi tool may call through `ctx.executeTool`:
/// `"any"` (pi's behaviour), `"own-extension"` (only its own extension's
/// tools), `"off"`; a refused call is an error result the calling tool sees,
/// and `ctx.tools` lists only what it may call.
#[tokio::test(flavor = "multi_thread")]
async fn nested_calls_follow_the_host_s_scope() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let alpha = extension(
        dir.path(),
        "alpha.ts",
        r#"import { Type } from 'typebox'
export default function (pi) {
  pi.registerTool({
    name: 'alpha_helper', label: 'h', description: 'h', parameters: Type.Object({}),
    exposure: 'deferred',
    async execute() { return { content: [{ type: 'text', text: 'alpha helper' }], details: undefined } },
  })
  pi.registerTool({
    name: 'alpha_runner', label: 'r', description: 'r', parameters: Type.Object({}),
    async execute(_id, _args, _signal, _update, ctx) {
      const said = []
      for (const name of ['alpha_helper', 'beta_helper']) {
        const out = await ctx.executeTool(name, {})
        said.push(`${name}: ${out.isError ? 'refused' : out.result.content[0].text}`)
      }
      const listed = ctx.tools.map((t) => t.name).filter((n) => n.endsWith('_helper')).sort()
      said.push(`listed: ${listed.join(' ') || 'none'}`)
      return { content: [{ type: 'text', text: said.join('; ') }], details: undefined }
    },
  })
}
"#,
    );
    let beta = extension(
        dir.path(),
        "beta.ts",
        r#"import { Type } from 'typebox'
export default function (pi) {
  pi.registerTool({
    name: 'beta_helper', label: 'h', description: 'h', parameters: Type.Object({}),
    exposure: 'deferred',
    async execute() { return { content: [{ type: 'text', text: 'beta helper' }], details: undefined } },
  })
}
"#,
    );
    for (mode, want) in [
        (
            None,
            "alpha_helper: alpha helper; beta_helper: beta helper; listed: alpha_helper beta_helper",
        ),
        (
            Some("own-extension"),
            "alpha_helper: alpha helper; beta_helper: refused; listed: alpha_helper",
        ),
        (
            Some("off"),
            "alpha_helper: refused; beta_helper: refused; listed: none",
        ),
    ] {
        let mut config = json!({ "extensions": [&alpha, &beta] });
        if let Some(mode) = mode {
            config["nestedCalls"] = json!(mode);
        }
        let host = host(&runtime).await;
        host.load(adapter_row(config)).await;
        let (agent, _) = agent(vec![call("alpha_runner", json!({})), text("done")]);
        let mut agent = agent.with_extension(host.bridge.extension());
        let (_, results) = run(&mut agent, "go").await;
        assert_eq!(
            results,
            vec![("alpha_runner".into(), want.to_string(), false)],
            "{mode:?}"
        );
        host.root.shutdown().await.unwrap();
    }

    // A value it does not know refuses the load.
    let host = host(&runtime).await;
    let failure = host
        .try_load(adapter_row(
            json!({ "extensions": [&alpha], "nestedCalls": "some" }),
        ))
        .await;
    assert!(failure.contains("nestedCalls"), "{failure}");
    host.root.shutdown().await.unwrap();
}

/// Under `"own-extension"`, a name two extensions registered belongs to the
/// winning (first) registration — the one whose code would run — in both
/// load orders; and under `"off"` an unknown name says the host turned nested
/// calls off.
#[tokio::test(flavor = "multi_thread")]
async fn own_extension_follows_the_winning_registration() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let helper = |who: &str| {
        format!(
            r#"pi.registerTool({{
    name: 'shared_helper', label: 'h', description: 'h', parameters: Type.Object({{}}),
    exposure: 'deferred',
    async execute() {{ return {{ content: [{{ type: 'text', text: '{who} shared' }}], details: undefined }} }},
  }})"#
        )
    };
    let alpha = extension(
        dir.path(),
        "alpha.ts",
        &format!(
            r#"import {{ Type }} from 'typebox'
export default function (pi) {{
  {}
  pi.registerTool({{
    name: 'alpha_runner', label: 'r', description: 'r', parameters: Type.Object({{}}),
    async execute(_id, _args, _signal, _update, ctx) {{
      const said = []
      for (const name of ['shared_helper', 'nope_helper']) {{
        const out = await ctx.executeTool(name, {{}})
        said.push(`${{name}}: ${{out.result.content[0].text}}`)
      }}
      said.push(`listed: ${{ctx.tools.map((t) => t.name).filter((n) => n.endsWith('_helper')).join(' ') || 'none'}}`)
      return {{ content: [{{ type: 'text', text: said.join('; ') }}], details: undefined }}
    }},
  }})
}}
"#,
            helper("alpha")
        ),
    );
    let beta = extension(
        dir.path(),
        "beta.ts",
        &format!(
            "import {{ Type }} from 'typebox'\nexport default function (pi) {{\n  {}\n}}\n",
            helper("beta")
        ),
    );
    for (order, mode, want) in [
        (
            [&beta, &alpha],
            "own-extension",
            &[
                "shared_helper: shared_helper belongs to another extension",
                "nope_helper: Tool nope_helper not found",
                "listed: none",
            ][..],
        ),
        (
            [&alpha, &beta],
            "own-extension",
            &[
                "shared_helper: alpha shared",
                "nope_helper: Tool nope_helper not found",
                "listed: shared_helper",
            ][..],
        ),
        (
            [&alpha, &beta],
            "off",
            &[
                "shared_helper: nested tool calls are turned off",
                "nope_helper: nested tool calls are turned off",
                "listed: none",
            ][..],
        ),
    ] {
        let host = host(&runtime).await;
        host.load(adapter_row(
            json!({ "extensions": order, "nestedCalls": mode }),
        ))
        .await;
        let (agent, _) = agent(vec![call("alpha_runner", json!({})), text("done")]);
        let mut agent = agent.with_extension(host.bridge.extension());
        let (_, results) = run(&mut agent, "go").await;
        assert_eq!(results.len(), 1, "{results:?}");
        for part in want {
            assert!(results[0].1.contains(part), "{mode} {part:?}: {results:?}");
        }
        host.root.shutdown().await.unwrap();
    }
}

/// A host `ui` that answers every request `true` and records it.
#[derive(Default)]
struct YesUi(Mutex<Vec<Value>>);

impl HostDispatch for YesUi {
    fn invoke(&self, method: &str, args: BridgeValue) -> Reply {
        match method {
            "request" => {
                let request = args.list()?.into_iter().next().unwrap().json()?;
                self.0.lock().unwrap().push(request);
                Ok(BridgeValue::Data(json!(true)))
            }
            "frontends" => Ok(BridgeValue::Data(json!(1))),
            _ => Ok(BridgeValue::Undefined),
        }
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "request": "async", "withdraw": "async", "frontends": "sync" }))
    }
}

/// `host-ui.ts` re-provides the host's `ui` as `pi-host-ui` (an undeclared
/// `ctx.provide`, which reaches rutis since 0.12): a pi extension's
/// `ctx.ui.confirm` then asks the host; without that row it answers as print
/// mode (`false`), and the policy denies.
#[tokio::test(flavor = "multi_thread")]
async fn host_ui_brings_pi_dialogs_to_the_host() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ext = extension(
        dir.path(),
        "ask.ts",
        r#"import { Type } from 'typebox'
export default function (pi) {
  pi.registerTool({
    name: 'pi_ask', label: 'a', description: 'a', parameters: Type.Object({}),
    async execute() { return { content: [{ type: 'text', text: 'asked and ran' }], details: undefined } },
  })
  pi.on('tool_call', async (event, ctx) => {
    if (event.toolName !== 'pi_ask') return
    if (!(await ctx.ui.confirm('Run pi_ask?', 'it is harmless'))) return { block: true, reason: 'not confirmed' }
  })
}
"#,
    );
    for with_host_ui in [true, false] {
        let ui = Arc::new(YesUi::default());
        let host = host_with_ui(&runtime, Some(ui.clone())).await;
        let mut rows = adapter_row(json!({ "extensions": [&ext] }));
        if with_host_ui {
            rows.as_array_mut()
                .unwrap()
                .push(json!({ "id": "host-ui", "name": pi_dir().join("host-ui.ts") }));
        }
        host.load(rows).await;
        let (agent, _) = agent(vec![call("pi_ask", json!({})), text("done")]);
        let mut agent = agent.with_extension(host.bridge.extension());
        let (_, results) = run(&mut agent, "go").await;
        let asked = ui.0.lock().unwrap().clone();
        if with_host_ui {
            assert_eq!(
                results,
                vec![("pi_ask".into(), "asked and ran".into(), false)]
            );
            assert_eq!(asked.len(), 1, "{asked:?}");
            assert_eq!(asked[0]["kind"], "confirm");
            assert_eq!(asked[0]["title"], "Run pi_ask?");
        } else {
            assert!(
                results[0].2 && results[0].1.contains("not confirmed"),
                "{results:?}"
            );
            assert!(asked.is_empty(), "{asked:?}");
        }
        host.root.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deciding_event_registered_mid_run_stops_that_run() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ext = extension(
        dir.path(),
        "late-context.ts",
        r#"import { Type } from 'typebox'
export default function (pi) {
  pi.registerTool({
    name: 'pi_arm', label: 'arm', description: 'registers a context handler', parameters: Type.Object({}),
    async execute() {
      pi.on('context', () => ({ messages: [] }))
      return { content: [{ type: 'text', text: 'armed' }], details: undefined }
    },
  })
}
"#,
    );
    let host = host(&runtime).await;
    host.load(adapter_row(json!({ "extensions": [ext] }))).await;
    let (agent, seen) = agent(vec![
        call("pi_arm", json!({})),
        call("pi_arm", json!({})),
        text("never requested"),
    ]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    // The handler appeared during the run: its next model request is stopped.
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the second request never went out"
    );
    let last = format!("{:?}", agent.messages().last());
    assert!(last.contains("context"), "{last}");
    host.root.shutdown().await.unwrap();
}

/// `message_end` and `context` handlers present at load are run on copies:
/// one that only reads works; one that would change what it got stops the
/// adapter before yoagent acts on the unchanged version.
#[tokio::test(flavor = "multi_thread")]
async fn message_end_and_context_handlers_are_checked_at_run_time() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("seen.txt");
    let ran = dir.path().join("ran.txt");
    // A tool, and handlers whose behaviour the file name picks.
    let source = |mode: &str| {
        format!(
            r#"import {{ appendFileSync }} from 'node:fs'
import {{ Type }} from 'typebox'
const MODE = {mode:?}
export default function (pi) {{
  pi.registerTool({{
    name: 'pi_note', label: 'note', description: 'notes', parameters: Type.Object({{}}),
    async execute() {{
      appendFileSync({ran:?}, 'ran\n')
      return {{ content: [{{ type: 'text', text: 'noted' }}], details: undefined }}
    }},
  }})
  pi.on('message_end', (e) => {{
    appendFileSync({log:?}, 'end ' + e.message.role + '\n')
    if (MODE === 'end' && e.message.role === 'assistant') return {{ message: {{ ...e.message, content: [] }} }}
    // Returning the message unchanged is not a rewrite.
    if (MODE === 'same') return {{ message: e.message }}
  }})
  pi.on('context', (e) => {{
    appendFileSync({log:?}, 'context ' + e.messages.length + '\n')
    if (MODE === 'context') e.messages.push({{ role: 'user', content: [{{ type: 'text', text: 'injected' }}], timestamp: 0 }})
  }})
}}
"#,
            ran = ran.display().to_string(),
            log = log.display().to_string(),
        )
    };
    let script = || vec![call("pi_note", json!({})), text("done")];

    // Observers (and a handler handing the message back unchanged): the run goes on.
    for mode in ["observe", "same"] {
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(&ran);
        let ext = extension(dir.path(), &format!("{mode}.ts"), &source(mode));
        let host = host(&runtime).await;
        host.load(adapter_row(json!({ "extensions": [ext] }))).await;
        let (agent, seen) = agent(script());
        let mut agent = agent.with_extension(host.bridge.extension());
        let (_, results) = run(&mut agent, "go").await;
        assert_eq!(results.len(), 1, "{mode}: {results:?}");
        assert!(!results[0].2, "{mode}: the tool ran: {results:?}");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "{mode}: both requests went out"
        );
        let seen = std::fs::read_to_string(&log).unwrap();
        // Each message once: prompt, the call, its result, the answer; and both requests' context.
        assert_eq!(
            seen.lines()
                .filter(|l| l.starts_with("end "))
                .collect::<Vec<_>>(),
            [
                "end user",
                "end assistant",
                "end toolResult",
                "end assistant"
            ],
            "{mode}: {seen}"
        );
        assert_eq!(
            seen.lines().filter(|l| l.starts_with("context ")).count(),
            2,
            "{mode}: {seen}"
        );
        host.root.shutdown().await.unwrap();
    }

    // A rewrite of the assistant message: denied before its tool runs.
    {
        let _ = std::fs::remove_file(&ran);
        let ext = extension(dir.path(), "end.ts", &source("end"));
        let host = host(&runtime).await;
        host.load(adapter_row(json!({ "extensions": [ext] }))).await;
        let (agent, _) = agent(script());
        let mut agent = agent.with_extension(host.bridge.extension());
        let (_, results) = run(&mut agent, "go").await;
        assert!(
            results[0].2 && results[0].1.contains("message_end"),
            "{results:?}"
        );
        assert!(!ran.exists(), "the tool never ran");
        host.root.shutdown().await.unwrap();
    }

    // A context changed in place: the first request never goes out.
    {
        let ext = extension(dir.path(), "context.ts", &source("context"));
        let host = host(&runtime).await;
        host.load(adapter_row(json!({ "extensions": [ext] }))).await;
        let (agent, seen) = agent(script());
        let mut agent = agent.with_extension(host.bridge.extension());
        run(&mut agent, "go").await;
        assert!(seen.lock().unwrap().is_empty(), "no request went out");
        let last = format!("{:?}", agent.messages().last());
        assert!(last.contains("context"), "{last}");
        host.root.shutdown().await.unwrap();
    }
}

/// A `message_end` check cut short by the hook's timeout does not pass: the
/// message is not marked judged, so the next call on it is checked again
/// (and here times out again) instead of going ahead unjudged.
/// rutis 0.11 sends an integer beyond ±(2^53−1) as a big integer; the adapter
/// compares arguments and messages in a JSON form, which must keep it rather
/// than throw (a throw denies the call, and with `message_end` handlers every
/// later call of the run).
#[tokio::test(flavor = "multi_thread")]
async fn a_big_integer_in_a_call_neither_denies_it_nor_later_calls() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("seen.txt");
    let source = format!(
        r#"import {{ appendFileSync }} from 'node:fs'
export default function (pi) {{
  pi.on('tool_call', (e) => {{
    appendFileSync({log:?}, typeof e.input.snowflake + '\n')
  }})
  pi.on('message_end', () => {{}})
  pi.on('context', () => {{}})
}}
"#,
        log = log.display().to_string(),
    );
    let ext = extension(dir.path(), "big.ts", &source);
    let host = host(&runtime).await;
    host.load(adapter_row(json!({ "extensions": [ext] }))).await;
    let snowflake = 1u64 << 60;
    let (agent, seen) = agent(vec![
        call(
            "bash",
            json!({"command": "echo first", "snowflake": snowflake}),
        ),
        call("bash", json!({"command": "echo second"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(BashTool::new())])
        .with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        !results[0].2 && results[0].1.contains("first"),
        "{results:?}"
    );
    assert!(
        !results[1].2 && results[1].1.contains("second"),
        "{results:?}"
    );
    assert_eq!(seen.lock().unwrap().len(), 3, "every request went out");
    // The policy saw the number exactly, as a JS bigint.
    let seen = std::fs::read_to_string(&log).unwrap();
    assert_eq!(seen.lines().next(), Some("bigint"), "{seen}");
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_end_check_cut_short_does_not_pass() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran.txt");
    let ext = extension(
        dir.path(),
        "slow.ts",
        &format!(
            r#"import {{ appendFileSync }} from 'node:fs'
import {{ Type }} from 'typebox'
export default function (pi) {{
  pi.registerTool({{
    name: 'pi_note', label: 'note', description: 'notes', parameters: Type.Object({{}}),
    async execute() {{
      appendFileSync({ran:?}, 'ran\n')
      return {{ content: [{{ type: 'text', text: 'noted' }}], details: undefined }}
    }},
  }})
  // Slow, and gives up when its call is cancelled.
  pi.on('message_end', (e, ctx) => new Promise((_, reject) => {{
    ctx.signal?.addEventListener('abort', () => reject(new Error('aborted')))
  }}))
}}
"#,
            ran = ran.display().to_string(),
        ),
    );
    let host = host(&runtime).await;
    host.load(adapter_row(json!({ "extensions": [ext] }))).await;
    let two_calls = MockResponse::ToolCalls(vec![
        MockToolCall {
            provider_metadata: None,
            name: "pi_note".into(),
            arguments: json!({}),
        },
        MockToolCall {
            provider_metadata: None,
            name: "pi_note".into(),
            arguments: json!({}),
        },
    ]);
    let (agent, _) = agent(vec![two_calls, text("done")]);
    let mut agent = agent
        .with_tool_execution(yoagent::ToolExecutionStrategy::Sequential)
        .with_extension(
            host.bridge
                .extension()
                .with_policy_timeout(Some(Duration::from_millis(500))),
        );
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results.iter().all(|r| r.2),
        "both calls denied: {results:?}"
    );
    assert!(!ran.exists(), "no tool ran");
    host.root.shutdown().await.unwrap();
}
