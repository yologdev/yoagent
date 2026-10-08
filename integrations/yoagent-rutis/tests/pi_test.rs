//! pi coding-agent extensions in yoagent, end to end: the adapter
//! `plugins/pi/pi-extensions-adapter.ts` loading pi extensions — the fixtures
//! `plugins/pi/fixture-extension.ts` and `plugins/pi/fixture-extra.ts`, and
//! small ones written to a temporary directory; no network — as a
//! rutis-loader row of a Node runtime.
//!
//! Needs Node 24+ and `npm ci` in `plugins/pi/`. Without them the test
//! prints `SKIPPED:` and passes; `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1` (CI) makes
//! it fail instead.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, Layer, Loader, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin,
    ServiceCatalog,
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

/// The Node runtime package, if Node 24+ and `npm ci` in `plugins/pi/` are there.
fn node_runtime() -> Option<PathBuf> {
    let skip = |why: String| -> Option<PathBuf> {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("the pi runtime is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1) but unavailable: {why}");
        }
        eprintln!("SKIPPED: the pi runtime is unavailable: {why}");
        None
    };
    let major = Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|m| m.parse::<u32>().ok())
        });
    match major {
        Some(major) if major >= 24 => {}
        Some(major) => return skip(format!("Node {major} found, 24+ needed")),
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
    Some(modules.join("@arcships/rutis-runtime"))
}

struct Host {
    root: Ctx,
    bridge: RutisBridge,
    loader: Loader,
}

async fn host(runtime: &Path) -> Host {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
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
        if report.failures.is_empty() {
            String::new()
        } else {
            format!("{:?}", report.failures)
        }
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
    let ended = wait_file(&slow, |t| t != "started").await;
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
    // terminate: true denied the call and stopped the run before its next request.
    assert!(
        result(15).2 && result(15).1.contains("stopping the run"),
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
            "export default function (pi) { pi.on('session_start', () => pi.appendEntry('x', {})) }\n",
            "session_start",
        ),
        (
            "context.ts",
            "export default function (pi) { pi.on('context', () => ({ messages: [] })) }\n",
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
    let ext = dir.path().join("context.ts");
    let host = host(&runtime).await;
    host.load(adapter_row(
        json!({ "extensions": [ext], "allowUnmapped": ["context"] }),
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
const tool = (name) => ({
  name, label: name, description: name, parameters: Type.Object({}),
  async execute() { return { content: [{ type: 'text', text: `${name} ran` }], details: undefined } },
})
export default function (pi) {
  pi.registerTool(tool('pi_x'))
  pi.registerTool(tool('pi_y'))
  pi.on('session_start', () => pi.setActiveTools(['read', 'pi_x']))
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
        ]),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![
            Box::new(BashTool::new()),
            Box::new(ReadFileTool::new()),
        ])
        .with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    let tools = seen.lock().unwrap()[0].tools.clone();
    assert!(
        tools.contains(&"pi_x".into()) && !tools.contains(&"pi_y".into()),
        "{tools:?}"
    );
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
