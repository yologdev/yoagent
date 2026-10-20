//! dsh (DeepSeek Harness) tool plugins as yoagent tools, end to end: the
//! adapter `plugins/dsh/dsh-tools-adapter.ts` and a fixture dsh tool plugin
//! (`plugins/dsh/fixture-tools.ts`, no network) loaded with
//! `@deepseek-ai/dsh-system-prompt` and `@deepseek-ai/dsh-tools` as
//! rutis-loader rows of one Node runtime; for dialogs, also
//! `@deepseek-ai/dsh-user-questions`, `@deepseek-ai/dsh-tool-ask-user` and
//! `plugins/dsh/host-dialogs.ts`, against a scripted host `ui` service.
//!
//! Needs Node 22.19+ and `npm ci` in `plugins/dsh/`. Without them the test
//! prints `SKIPPED:` and passes; `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1` (CI) makes
//! it fail instead.
#![cfg(unix)]

mod common;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::Ctx;
use rutis_bridge::runtime::{Launcher, LocalRuntime};
use rutis_bridge::session::{
    host_key, Error as BridgeError, HostDispatch, Reply, Value as BridgeValue,
};
use rutis_loader::{
    Chain, Layer, Loader, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin,
    ServiceCatalog,
};
use serde_json::{json, Value};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::{AgentEvent, AgentMessage, Content, Message};
use yoagent_rutis::RutisBridge;

fn dsh_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/dsh")
}

/// The Node runtime package, if Node 22.19+ and `npm ci` in `plugins/dsh/` are there.
fn node_runtime() -> Option<PathBuf> {
    let skip = |why: String| -> Option<PathBuf> {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("the dsh runtime is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1) but unavailable: {why}");
        }
        eprintln!("SKIPPED: the dsh runtime is unavailable: {why}");
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
    let modules = dsh_dir().join("node_modules");
    for package in ["@arcships/rutis-runtime", "@deepseek-ai/dsh-tools"] {
        if !modules.join(package).join("package.json").exists() {
            return skip(format!(
                "{package} missing from {}: run `npm ci` in plugins/dsh/",
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
            "@arcships/rutis-runtime is {version:?}, 0.12 needed: run `npm ci` in plugins/dsh/"
        ));
    }
    Some(runtime)
}

struct Host {
    root: Ctx,
    bridge: RutisBridge,
    loader: Loader,
}

/// A host `ui` service (what yoagent-frontend provides) that answers from a
/// script, in order, and records what it was asked.
#[derive(Default)]
struct ScriptedUi {
    answers: Mutex<VecDeque<Value>>,
    asked: Mutex<Vec<Value>>,
    /// What `frontends()` reports.
    frontends: usize,
    /// Every request fails, as a host that broke would.
    fail: AtomicBool,
}

impl HostDispatch for ScriptedUi {
    fn invoke(&self, method: &str, args: BridgeValue) -> Reply {
        match method {
            "request" => {
                let request = args.list()?.into_iter().next().unwrap().json()?;
                self.asked.lock().unwrap().push(request);
                if self.fail.load(Ordering::SeqCst) {
                    return Err(BridgeError::Value("the host's ui broke".into()));
                }
                let answer = self.answers.lock().unwrap().pop_front();
                Ok(BridgeValue::Data(answer.unwrap_or(Value::Null)))
            }
            "frontends" => Ok(BridgeValue::Data(json!(self.frontends))),
            _ => Ok(BridgeValue::Undefined),
        }
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "request": "async", "withdraw": "async", "frontends": "sync" }))
    }
}

async fn host(runtime: &Path, ui: Option<Arc<ScriptedUi>>) -> Host {
    host_with(runtime, ui, &[]).await
}

/// [`host`], the DSH runtime's process given `env` on top of what it inherits.
async fn host_with(runtime: &Path, ui: Option<Arc<ScriptedUi>>, env: &[(&str, &Path)]) -> Host {
    host_serving(runtime, ui, env, Vec::new()).await
}

/// [`host_serving`] over `plugins/dsh`.
async fn host_serving(
    runtime: &Path,
    ui: Option<Arc<ScriptedUi>>,
    env: &[(&str, &Path)],
    services: Vec<(&str, Arc<dyn HostDispatch>)>,
) -> Host {
    host_in(&dsh_dir(), runtime, ui, env, services).await
}

/// [`host_with`], also sharing these host services into the runtime, with
/// the DSH runtime's packages those of `dir` (`plugins/dsh` or a copy).
async fn host_in(
    dir: &Path,
    runtime: &Path,
    ui: Option<Arc<ScriptedUi>>,
    env: &[(&str, &Path)],
    services: Vec<(&str, Arc<dyn HostDispatch>)>,
) -> Host {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    for (name, service) in services {
        catalog.register_shared(name);
        root.provide_as::<dyn HostDispatch>(host_key(name), service)
            .unwrap();
    }
    if let Some(ui) = ui {
        catalog.register_shared("ui");
        root.provide_as::<dyn HostDispatch>(host_key("ui"), ui)
            .unwrap();
    }
    let launcher = env
        .iter()
        .fold(Launcher::node(runtime), |launcher, (name, value)| {
            launcher.env(*name, *value)
        });
    let node = LocalRuntime::launcher("node", launcher, dir.join("package.json"));
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
    async fn load(&self, rows: Value) {
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
        let report = self
            .loader
            .reconcile(vec![Layer::new("rows", patches)], None)
            .await
            .unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
        let registry = self.bridge.registry().clone();
        tokio::time::timeout(Duration::from_secs(60), async {
            while !registry.handlers().iter().any(|h| h.name() == "dsh-tools") {
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
async fn dsh_tools_reach_a_yoagent_agent_through_the_adapter() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let abort_file = scratch.path().join("slow.txt");
    let host = host(&runtime, None).await;
    host.load(json!([
        { "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" },
        { "id": "tools", "name": "@deepseek-ai/dsh-tools" },
        {
            "id": "fixture",
            "name": dsh_dir().join("fixture-tools.ts"),
            "config": { "abortFile": abort_file },
        },
        { "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") },
    ]))
    .await;

    let (agent, seen) = agent(vec![
        calls(&[
            ("fixture_echo", json!({"text": "hi"})),
            ("fixture_fail", json!({"why": "nope"})),
            // Beyond ±(2^53−1): a `bigint` in the plugin since rutis 0.11.
            (
                "fixture_echo",
                json!({"text": "big", "id": (1u64 << 60) + 1}),
            ),
        ]),
        text("done"),
        call("fixture_slow", json!({})),
        text("never sent"),
    ]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (_, results) = tokio::time::timeout(Duration::from_secs(60), run(&mut agent, "go"))
        .await
        .expect("the run finishes");

    // The adapter offered the fixture's tools...
    let seen_now = seen.lock().unwrap().clone();
    for tool in ["fixture_echo", "fixture_fail", "fixture_slow"] {
        assert!(
            seen_now[0].tools.contains(&tool.to_string()),
            "{seen_now:?}"
        );
    }
    // ...a call returns its text...
    assert_eq!(
        results[0],
        ("fixture_echo".into(), "echo: hi".into(), false),
        "{results:?}"
    );
    // ...an `isError` result is an error tool result...
    let (name, text, is_error) = &results[1];
    assert_eq!(name, "fixture_fail");
    assert!(
        *is_error && text.contains("fixture failure: nope"),
        "{results:?}"
    );
    // ...dsh refuses arguments holding an integer it cannot keep exactly
    // (before rutis 0.11 it arrived as a JS number, here rounded, and the
    // tool ran)...
    let (name, text, is_error) = &results[2];
    assert_eq!(name, "fixture_echo");
    assert!(
        *is_error && text.contains("losslessly JSON-serializable"),
        "{results:?}"
    );
    // ...and the plugin's prompt section reached the request as a note,
    // without the harness's own sections.
    let note = &seen_now[0].last_user;
    assert!(
        note.contains("[Guidance from dsh plugins]")
            && note.contains("Fixture guidance: prefer fixture_echo for echoing."),
        "{note}"
    );
    assert!(!note.contains("DeepSeek Harness"), "{note}");
    assert!(
        !format!("{:?}", agent.messages()).contains("Fixture guidance"),
        "the note is never stored"
    );

    // Cancelling the run aborts the dsh tool through its signal.
    let mut rx = agent.prompt("slow").await;
    wait_file(&abort_file, |t| t == "started").await;
    agent.abort();
    tokio::time::timeout(Duration::from_secs(30), async {
        while rx.recv().await.is_some() {}
    })
    .await
    .expect("the cancelled run ends");
    agent.finish().await;
    // Not empty: writeFileSync truncates before it writes.
    let ended = wait_file(&abort_file, |t| t != "started" && !t.is_empty()).await;
    assert_eq!(ended, "aborted: AbortError");
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dsh_images_arrive_as_images_when_an_attachment_store_is_loaded() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    for with_store in [true, false] {
        let host = host(&runtime, None).await;
        let mut rows = vec![
            json!({ "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
            json!({ "id": "tools", "name": "@deepseek-ai/dsh-tools" }),
            json!({ "id": "fixture", "name": dsh_dir().join("fixture-tools.ts") }),
            json!({ "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") }),
        ];
        if with_store {
            // Loaded after the adapter: it is looked up when an image is read.
            rows.push(
                json!({ "id": "attachments", "name": dsh_dir().join("fixture-attachments.ts") }),
            );
        }
        host.load(Value::Array(rows)).await;
        let (agent, _) = agent(vec![call("fixture_dot", json!({})), text("done")]);
        let mut agent = agent.with_extension(host.bridge.extension());
        run(&mut agent, "go").await;
        let content = agent
            .messages()
            .iter()
            .find_map(|m| match m {
                AgentMessage::Llm(Message::ToolResult {
                    tool_name, content, ..
                }) if tool_name == "fixture_dot" => Some(content.clone()),
                _ => None,
            })
            .expect("fixture_dot ran");
        assert!(
            matches!(&content[0], Content::Text { text } if text == "a dot"),
            "{content:?}"
        );
        // dsh's offloaded image and the oversize one stay text, store or not.
        assert!(
            matches!(&content[2], Content::Text { text } if text.contains("offloaded.png: offloaded")),
            "{content:?}"
        );
        assert!(
            matches!(&content[3], Content::Text { text } if text.contains("huge.png: too large")),
            "{content:?}"
        );
        if with_store {
            // The attachment's bytes, read from the store, as a yoagent image.
            assert!(
                matches!(&content[1], Content::Image { data, mime_type }
                    if mime_type == "image/png" && data.starts_with("iVBORw0KGgo")),
                "{content:?}"
            );
        } else {
            assert!(
                matches!(&content[1], Content::Text { text } if text.contains("not available")),
                "{content:?}"
            );
        }
        host.root.shutdown().await.unwrap();
    }
}

fn tool_ends(events: &[AgentEvent]) -> Vec<(String, bool, Value)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                result,
                ..
            } => Some((tool_name.clone(), *is_error, result.details.clone())),
            _ => None,
        })
        .collect()
}

/// With a host `ui` and `host-dialogs.ts`, dsh asks the user: an approval its
/// policy requires, and `ask_user_question`. A tool's presenters reach the
/// result as `details.view`.
#[tokio::test(flavor = "multi_thread")]
async fn dsh_asks_the_user_through_a_host_ui_and_presents_its_calls() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let ui = Arc::new(ScriptedUi {
        frontends: 1,
        ..ScriptedUi::default()
    });
    const OTHER: &str = "Other (type an answer)";
    ui.answers.lock().unwrap().extend([
        json!(true),
        json!(false),
        json!(OTHER),
        json!("green"),
        json!(["c", "zzz", "a"]),
        json!("   "),
        json!(OTHER),
        json!("a"),
    ]);
    let host = dialog_host(&runtime, ui.clone()).await;

    let questions = json!({ "questions": [
        { "id": "colour", "question": "Which colour?", "options": [{ "label": "red" }, { "label": "blue", "description": "the sky" }] },
        { "id": "letters", "question": "Which letters?", "multi_select": true, "options": [{ "label": "a" }, { "label": "b" }, { "label": "c" }] },
        { "id": "name", "header": "About you", "question": "Your name?" },
        { "id": "own", "question": "Own other?", "options": [{ "label": "x" }, { "label": OTHER }] },
        { "id": "twice", "question": "Twice?", "options": [{ "label": "a" }, { "label": "a" }] },
    ]});
    let (agent, _) = agent(vec![
        call("fixture_guarded", json!({})),
        call("fixture_guarded", json!({ "path": "notes.txt" })),
        call("ask_user_question", questions),
        call("fixture_echo", json!({ "text": "hi" })),
        text("done"),
    ]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (events, results) = tokio::time::timeout(Duration::from_secs(60), run(&mut agent, "go"))
        .await
        .expect("the run finishes");

    // The policy's `ask` reached the user: yes ran the tool, no denied it.
    assert_eq!(
        results[0],
        ("fixture_guarded".into(), "guarded ran".into(), false),
        "{results:?}"
    );
    let (_, text, is_error) = &results[1];
    assert!(
        *is_error && text.contains("was not approved"),
        "{results:?}"
    );
    let asked = ui.asked.lock().unwrap().clone();
    for confirm in &asked[..2] {
        assert_eq!(confirm["kind"], "confirm", "{asked:?}");
        let message = confirm["message"].as_str().unwrap();
        assert!(message.contains("fixture_guarded needs a yes"), "{message}");
        assert!(
            confirm["key"].as_str().unwrap().starts_with("dsh-"),
            "withdrawable"
        );
    }
    assert!(
        asked[1]["message"].as_str().unwrap().contains("notes.txt"),
        "the arguments are shown: {asked:?}"
    );

    // ask_user_question: "Other" asks for text, a multiple select keeps known
    // labels in option order, a blank input is a skip, an option already
    // named like "Other" is not doubled, and labels are asked once each.
    assert_eq!(asked[2]["kind"], "select");
    assert_eq!(asked[2]["options"], json!(["red", "blue", OTHER]));
    assert!(asked[2]["message"]
        .as_str()
        .unwrap()
        .contains("blue: the sky"));
    assert_eq!(asked[3]["kind"], "input", "Other → typed text");
    assert_eq!(asked[4]["multiple"], true);
    assert_eq!(asked[5]["title"], "About you: Your name?");
    assert_eq!(asked[6]["options"], json!(["x", OTHER]));
    assert_eq!(asked[7]["options"], json!(["a", OTHER]));
    assert_eq!(asked.len(), 8, "{asked:?}");
    let (name, text, is_error) = &results[2];
    assert_eq!(name, "ask_user_question");
    assert!(!is_error, "{text}");
    let answers: Value = serde_json::from_str(text).expect("the answers as JSON");
    assert_eq!(
        answers["answers"],
        json!([
            { "id": "colour", "selected": [], "custom": "green" },
            { "id": "letters", "selected": ["a", "c"] },
            { "id": "name", "selected": [] },
            { "id": "own", "selected": [OTHER] },
            { "id": "twice", "selected": ["a"] },
        ])
    );

    // The echo's own presenters, as details.view.
    let ends = tool_ends(&events);
    let (_, _, details) = ends.iter().find(|(n, _, _)| n == "fixture_echo").unwrap();
    assert_eq!(
        details["view"],
        json!({
            "call": { "card": "terminal", "title": "echo hi", "description": "Echo a text back" },
            "result": { "card": "terminal", "output": "echo: hi", "exitCode": 0 },
        })
    );
    host.root.shutdown().await.unwrap();
}

/// The rows a dialog test loads, against `ui`.
async fn dialog_host(runtime: &Path, ui: Arc<ScriptedUi>) -> Host {
    let host = host(runtime, Some(ui)).await;
    host.load(json!([
        { "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" },
        { "id": "tools", "name": "@deepseek-ai/dsh-tools" },
        { "id": "questions", "name": "@deepseek-ai/dsh-user-questions" },
        { "id": "ask-user", "name": "@deepseek-ai/dsh-tool-ask-user" },
        { "id": "fixture", "name": dsh_dir().join("fixture-tools.ts") },
        { "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") },
        { "id": "host-dialogs", "name": dsh_dir().join("host-dialogs.ts") },
    ]))
    .await;
    host
}

/// A host that fails while asking never lets a call through, and a question
/// nobody answered is reported as such, never as an empty answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_or_unanswered_dsh_dialog_is_never_an_answer() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let ui = Arc::new(ScriptedUi {
        frontends: 1,
        ..ScriptedUi::default()
    });
    ui.fail.store(true, Ordering::SeqCst);
    let host = dialog_host(&runtime, ui.clone()).await;
    let question = json!({ "questions": [{ "id": "q", "question": "Which?", "options": [{ "label": "a" }] }] });
    let (agent, _) = agent(vec![
        call("fixture_guarded", json!({})),
        call("ask_user_question", question.clone()),
        text("one"),
        call("ask_user_question", question),
        text("two"),
    ]);
    let mut agent = agent.with_extension(host.bridge.extension());

    let (_, results) = run(&mut agent, "the host breaks").await;
    let (_, text, is_error) = &results[0];
    assert!(
        *is_error && text.contains("asking the user failed"),
        "denied, never allowed: {results:?}"
    );
    let (_, text, is_error) = &results[1];
    assert!(
        *is_error && text.contains("asking the user failed"),
        "{results:?}"
    );

    // Working again, but nobody answers: the asker gets null.
    ui.fail.store(false, Ordering::SeqCst);
    let (_, results) = run(&mut agent, "nobody answers").await;
    let (_, text, is_error) = &results[0];
    assert!(
        *is_error && text.contains(r#"did not answer question "q""#),
        "{results:?}"
    );
    host.root.shutdown().await.unwrap();
}

/// A tool's card never fails its call: a presenter that throws or returns
/// what JSON cannot carry is cleaned or left out, a card over the cap is
/// left out, and the runtime goes on.
#[tokio::test(flavor = "multi_thread")]
async fn a_dsh_card_never_fails_its_call() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let host = host(&runtime, None).await;
    host.load(json!([
        { "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" },
        { "id": "tools", "name": "@deepseek-ai/dsh-tools" },
        { "id": "fixture", "name": dsh_dir().join("fixture-tools.ts") },
        { "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") },
    ]))
    .await;
    let (agent, _) = agent(vec![
        call("fixture_odd", json!({})),
        call("fixture_huge", json!({})),
        call("fixture_fail", json!({ "why": "no card" })),
        call("fixture_echo", json!({ "text": "still here" })),
        text("done"),
    ]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (events, results) = run(&mut agent, "go").await;
    assert_eq!(results[0], ("fixture_odd".into(), "odd ran".into(), false));
    assert_eq!(
        results[1],
        ("fixture_huge".into(), "huge ran".into(), false)
    );
    assert_eq!(results[3].1, "echo: still here", "the runtime went on");
    let ends = tool_ends(&events);
    assert_eq!(
        ends[0].2,
        json!({ "view": { "result": {
            "card": "generic",
            "title": "odd",
            "big": "12345678901234567890",
            "nan": null,
            "surrogate": "x\u{FFFD}",
            "k\u{FFFD}": 1,
            "when": "1970-01-01T00:00:00.000Z",
            "content": [{ "type": "text", "text": "t" }, { "type": "text", "text": "[image block]" }],
        } } }),
        "the throwing presentCall left no call card"
    );
    assert_eq!(ends[1].2, Value::Null, "over the cap: no card");
    assert_eq!(ends[2].2, Value::Null, "an error result: no card");
    host.root.shutdown().await.unwrap();
}

/// Without a host `ui`, or with one but no frontend attached, dsh stays as
/// it is on its own: an `ask` is denied, and nobody is asked.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_frontend_a_dsh_ask_is_denied() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    for ui in [None, Some(Arc::new(ScriptedUi::default()))] {
        let host = host(&runtime, ui.clone()).await;
        let mut rows = vec![
            json!({ "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
            json!({ "id": "tools", "name": "@deepseek-ai/dsh-tools" }),
            json!({ "id": "fixture", "name": dsh_dir().join("fixture-tools.ts") }),
            json!({ "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") }),
        ];
        if ui.is_some() {
            rows.push(json!({ "id": "host-dialogs", "name": dsh_dir().join("host-dialogs.ts") }));
        }
        host.load(Value::Array(rows)).await;
        let (agent, _) = agent(vec![call("fixture_guarded", json!({})), text("done")]);
        let mut agent = agent.with_extension(host.bridge.extension());
        let (_, results) = run(&mut agent, "go").await;
        let (_, text, is_error) = &results[0];
        assert!(
            *is_error && text.contains("fixture_guarded needs a yes"),
            "{results:?}"
        );
        if let Some(ui) = ui {
            assert!(
                ui.asked.lock().unwrap().is_empty(),
                "no frontend: not asked"
            );
        }
        host.root.shutdown().await.unwrap();
    }
}

/// `plugins/dsh/host-services.json`: DSH's own services start headless, and
/// with `DSH_HOME` / `DSH_AGENTS_HOME` set they use that home, not the
/// user's: a plugin reads a key stored there through DSH's credentials
/// service, and nothing is created under `HOME` (pointed at an empty dir).
/// The services DSH plugins wait for exist in the runtime (checked from
/// inside it, as a plugin sees them).
#[tokio::test(flavor = "multi_thread")]
async fn dsh_host_services_start_headless_in_their_own_dsh_home() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("dsh-home");
    let agents = dir.path().join("agents");
    let user = dir.path().join("user");
    for d in [&home, &user] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        home.join(".credentials.yaml"),
        "version: 1\nrefs:\n  YOAGENT_TEST_KEY: from-the-given-home\n",
    )
    .unwrap();
    // DSH refuses a credentials file others can read.
    std::fs::set_permissions(
        home.join(".credentials.yaml"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let services_out = dir.path().join("services.json");
    let key_out = dir.path().join("key.txt");
    // A plain Cordis plugin reading one key through the credentials service.
    let reader = dir.path().join("read-key.mjs");
    std::fs::write(
        &reader,
        format!(
            "import {{ writeFileSync }} from 'node:fs'\n\
             export const name = 'read-key'\n\
             export const inject = ['credentials']\n\
             export async function apply(ctx) {{\n\
               const found = await ctx.credentials.resolve('YOAGENT_TEST_KEY')\n\
               writeFileSync({:?}, String(found?.value) + ' ' + String(found?.source))\n\
             }}\n",
            key_out.display().to_string()
        ),
    )
    .unwrap();
    let host = host_with(
        &runtime,
        None,
        &[
            ("DSH_HOME", &home),
            ("DSH_AGENTS_HOME", &agents),
            ("HOME", &user),
        ],
    )
    .await;
    let services = [
        "fs",
        "attachments",
        "credentials",
        "storage",
        "sessionPersistence",
        "skills",
    ];
    let mut rows: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(dsh_dir().join("host-services.json")).unwrap(),
    )
    .unwrap();
    rows.extend([
        json!({ "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
        json!({ "id": "tools", "name": "@deepseek-ai/dsh-tools" }),
        json!({ "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") }),
        json!({ "id": "read-key", "name": reader }),
        json!({
            "id": "probe-services",
            "name": dsh_dir().join("probe-services.ts"),
            "config": { "names": services, "out": services_out, "waitMs": 1000 },
        }),
    ]);
    host.load(Value::Array(rows)).await;
    let report: Value =
        serde_json::from_str(&wait_file(&services_out, |t| !t.is_empty()).await).unwrap();
    assert_eq!(report["missing"], json!([]), "{report}");
    assert_eq!(
        report["present"].as_array().map(Vec::len),
        Some(services.len()),
        "{report}"
    );
    assert_eq!(
        wait_file(&key_out, |t| !t.is_empty()).await,
        "from-the-given-home file",
        "the key comes from the given DSH_HOME"
    );
    let created: Vec<_> = std::fs::read_dir(&user)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(created.is_empty(), "nothing under HOME: {created:?}");
    host.root.shutdown().await.unwrap();
}

/// A dsh plugin that needs DSH's host services (most popular ones do) runs
/// and reaches the agent only with `host-services.json` loaded; without it
/// the plugin never starts and its tool is never offered.
#[tokio::test(flavor = "multi_thread")]
async fn a_dsh_plugin_needing_host_services_runs_only_with_them() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    for with_services in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("dsh-home");
        std::fs::create_dir_all(&home).unwrap();
        let credentials = home.join(".credentials.yaml");
        std::fs::write(
            &credentials,
            "version: 1\nrefs:\n  YOAGENT_TEST_KEY: from-dsh-home\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &credentials,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        let agents = dir.path().join("agents");
        let ready = dir.path().join("ready.txt");
        let host = host_with(
            &runtime,
            None,
            &[("DSH_HOME", &home), ("DSH_AGENTS_HOME", &agents)],
        )
        .await;
        let mut rows: Vec<Value> = if with_services {
            serde_json::from_str(
                &std::fs::read_to_string(dsh_dir().join("host-services.json")).unwrap(),
            )
            .unwrap()
        } else {
            Vec::new()
        };
        rows.extend([
            json!({ "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
            json!({ "id": "tools", "name": "@deepseek-ai/dsh-tools" }),
            json!({
                "id": "needs-services",
                "name": dsh_dir().join("fixture-needs-services.ts"),
                "config": { "ready": ready },
            }),
            json!({ "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") }),
        ]);
        host.load(Value::Array(rows)).await;
        if with_services {
            wait_file(&ready, |t| t == "started").await;
        } else {
            // Long enough for a plugin with its services to have started.
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert!(
                !ready.exists(),
                "without the host services it must not start"
            );
        }
        let (agent, _) = agent(vec![call("fixture_services", json!({})), text("done")]);
        let mut agent = agent.with_extension(host.bridge.extension());
        let (_, results) = run(&mut agent, "go").await;
        let (_, text, is_error) = &results[0];
        if with_services {
            assert!(
                !is_error && text == "credentials: from-dsh-home",
                "{results:?}"
            );
        } else {
            assert!(*is_error && text.contains("not found"), "{results:?}");
        }
        host.root.shutdown().await.unwrap();
    }
}

/// What a host gives `dsh-host.ts` (yobuild's `host_session` and
/// `host_workspace`): the session's id and generation, steering it is
/// woken with, and the workspace.
struct FakeSession {
    generation: AtomicU64,
    woken: Mutex<Vec<String>>,
}

impl HostDispatch for FakeSession {
    fn invoke(&self, method: &str, args: BridgeValue) -> Reply {
        match method {
            "info" => Ok(BridgeValue::Data(
                json!({ "id": "host-session", "generation": self.generation.load(Ordering::SeqCst) }),
            )),
            "wake" => {
                let text = args.list()?.into_iter().next().unwrap().json()?;
                self.woken
                    .lock()
                    .unwrap()
                    .push(text.as_str().unwrap_or_default().to_owned());
                Ok(BridgeValue::Undefined)
            }
            _ => Ok(BridgeValue::Undefined),
        }
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "info": "async", "wake": "async" }))
    }
}

struct FakeWorkspace(PathBuf);

impl HostDispatch for FakeWorkspace {
    fn invoke(&self, method: &str, _args: BridgeValue) -> Reply {
        match method {
            "cwd" => Ok(BridgeValue::Data(json!(self.0))),
            _ => Ok(BridgeValue::Undefined),
        }
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "cwd": "async" }))
    }
}

/// What a host with the stand-in loads: DSH's prompt and tool registry, then
/// `adapters::dsh_host_rows` (`host-services.json` with `dsh-host.json`'s
/// rows in), then the fixtures, the adapter and the dialogs.
fn stand_in_rows(sessions: &Path) -> Value {
    let mut rows = vec![
        json!({ "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
        json!({ "id": "tools", "name": "@deepseek-ai/dsh-tools" }),
    ];
    rows.extend(yoagent_rutis::adapters::dsh_host_rows(&dsh_dir(), sessions).unwrap());
    rows.extend([
        json!({ "id": "fixture", "name": dsh_dir().join("fixture-tools.ts") }),
        json!({ "id": "fixture-agent", "name": dsh_dir().join("fixture-needs-agent.ts") }),
        json!({ "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") }),
        json!({ "id": "dialogs", "name": dsh_dir().join("host-dialogs.ts") }),
    ]);
    Value::Array(rows)
}

/// A host with the stand-in: fake `host_session` / `host_workspace`, a
/// scripted `ui`, DSH's homes under `dir`.
async fn stand_in_host(
    runtime: &Path,
    dir: &Path,
    session: Arc<FakeSession>,
    ui: Arc<ScriptedUi>,
) -> Host {
    let work = dir.join("work");
    let home = dir.join("dsh-home");
    for d in [&work, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let host = host_serving(
        runtime,
        Some(ui),
        &[
            ("DSH_HOME", &home),
            ("DSH_AGENTS_HOME", &home),
            ("HOME", &home),
        ],
        vec![
            ("host_session", session as Arc<dyn HostDispatch>),
            ("host_workspace", Arc::new(FakeWorkspace(work))),
        ],
    )
    .await;
    host.load(stand_in_rows(&dir.join("sessions"))).await;
    host
}

fn fake_session() -> Arc<FakeSession> {
    Arc::new(FakeSession {
        generation: AtomicU64::new(1),
        woken: Mutex::new(Vec::new()),
    })
}

/// `fixture_turns`' answer: turns started, and each turn's end reason.
fn turns(text: &str) -> (u64, Vec<String>) {
    let v: Value = serde_json::from_str(text).unwrap_or_else(|_| panic!("{text}"));
    (
        v["starts"].as_u64().unwrap(),
        serde_json::from_value(v["ends"].clone()).unwrap(),
    )
}

/// With `dsh-host.ts` loaded (rows from `adapters::dsh_host_rows`), each call
/// carries a stand-in DSH agent: a dsh tool that needs one works in the
/// host's workspace; each run is one DSH turn, ended `completed` at its
/// finish; steering goes to the host's `host_session.wake`; a policy's
/// `ask` goes through DSH's own approval service to the host's `ui`, with
/// the call's arguments (yes → runs, no → rejected); a host reset (a new
/// generation) gives a new agent; the session logs are written.
#[tokio::test(flavor = "multi_thread")]
async fn a_stand_in_agent_gives_dsh_plugins_a_workspace_turns_and_approvals() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let session = fake_session();
    let ui = Arc::new(ScriptedUi {
        frontends: 1,
        ..ScriptedUi::default()
    });
    // Allow once; then Deny; then no answer (the script runs out: `null`).
    ui.answers
        .lock()
        .unwrap()
        .extend([json!("Allow once"), json!("Deny")]);
    let host = stand_in_host(&runtime, dir.path(), session.clone(), ui.clone()).await;

    let mut agent = agent_again(
        &host,
        vec![
            call("fixture_where", json!({})),
            call("fixture_steer", json!({ "text": "look again" })),
            call("fixture_guarded", json!({ "path": "notes.txt" })),
            call("fixture_turns", json!({})),
            text("done"),
        ],
    );
    let (_, results) = tokio::time::timeout(Duration::from_secs(60), run(&mut agent, "go"))
        .await
        .expect("the run finishes");
    let (_, whereabouts, failed) = &results[0];
    assert!(
        !failed && whereabouts.ends_with(work.to_str().unwrap()),
        "{results:?}"
    );
    let first_agent = whereabouts.split(' ').next().unwrap().to_owned();
    assert!(
        first_agent.starts_with("yoagent-"),
        "a unique session id: {first_agent}"
    );
    assert_eq!(results[1].1, "steered", "{results:?}");
    // The policy's `ask` went through DSH's approval service to the user: yes.
    assert_eq!(
        results[2],
        ("fixture_guarded".into(), "guarded ran".into(), false),
        "{results:?}"
    );
    let asked = ui.asked.lock().unwrap().clone();
    assert_eq!(asked[0]["kind"], "select", "{asked:?}");
    assert_eq!(
        asked[0]["options"],
        json!(["Allow once", "Deny"]),
        "{asked:?}"
    );
    let shown = asked[0]["message"].as_str().unwrap();
    assert!(shown.contains("fixture_guarded needs a yes"), "{shown}");
    assert!(
        shown.contains("notes.txt"),
        "the arguments are shown: {shown}"
    );
    assert_eq!(turns(&results[3].1), (1, vec![]), "{results:?}");
    // Steering is sent to the host as it comes (fire and forget).
    tokio::time::timeout(Duration::from_secs(10), async {
        while session.woken.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the steering reaches the host");
    assert_eq!(*session.woken.lock().unwrap(), ["look again"]);

    // A second run: the first turn ended `completed`; the user denies, then
    // does not answer — not a rejection, but no approval.
    let mut agent = agent_again(
        &host,
        vec![
            call("fixture_guarded", json!({})),
            call("fixture_turns", json!({})),
            call("fixture_where", json!({})),
            call("fixture_guarded", json!({})),
            text("done"),
        ],
    );
    let (_, results) = run(&mut agent, "go").await;
    assert!(
        results[0].2 && results[0].1.contains("rejected"),
        "{results:?}"
    );
    assert_eq!(
        turns(&results[1].1),
        (2, vec!["completed".into()]),
        "{results:?}"
    );
    assert!(results[2].1.starts_with(&first_agent), "{results:?}");
    assert!(
        results[3].2 && results[3].1.contains("no approval channel"),
        "no answer is unavailable, not a rejection: {results:?}"
    );

    // The host reset: a new generation, a new agent with a turn of its own.
    session.generation.store(2, Ordering::SeqCst);
    let mut agent = agent_again(
        &host,
        vec![
            call("fixture_where", json!({})),
            call("fixture_turns", json!({})),
            text("done"),
        ],
    );
    let (_, results) = run(&mut agent, "go").await;
    assert!(!results[0].2, "{results:?}");
    assert!(
        !results[0].1.starts_with(&first_agent),
        "a new agent: {results:?}"
    );
    assert_eq!(turns(&results[1].1), (1, vec![]), "{results:?}");

    host.root.shutdown().await.unwrap();
    // Both generations' sessions are in the log, with their turns and the
    // approvals' audit pairs (read back through DSH's own jsonl backend).
    let logs = read_logs(&dir.path().join("sessions"));
    let first: Vec<String> = serde_json::from_value(logs[&first_agent].clone()).unwrap();
    for want in [
        "turn/start",
        "approval/asked",
        "approval/decided",
        "turn/end",
    ] {
        assert!(first.iter().any(|t| t == want), "{want}: {first:?}");
    }
    assert_eq!(
        first.iter().filter(|t| *t == "turn/end").count(),
        2,
        "{first:?}"
    );
    assert_eq!(
        first.iter().filter(|t| *t == "approval/decided").count(),
        3,
        "{first:?}"
    );
    assert_eq!(
        logs.as_object().unwrap().len(),
        2,
        "two generations: {logs}"
    );
}

/// The event types of every session stored under `sessions`, by session id,
/// read with DSH's jsonl backend (in a Node process over `plugins/dsh`).
fn read_logs(sessions: &Path) -> Value {
    let script = format!(
        r#"
import {{ Context }} from '@deepseek-ai/cordis'
const root = new Context()
const m = await import('@deepseek-ai/dsh-session-persistence-jsonl')
await root.plugin(m.default && !m.apply ? m.default : m, {{ root: {root:?} }})
const store = root.get('sessionPersistence')
const out = {{}}
for (const snap of await store.list()) {{
  const handle = await store.open(snap.header.id, 'read')
  const read = await handle.read()
  out[snap.header.id] = read.events.map((e) => e.type)
  await handle[Symbol.asyncDispose]?.()
}}
console.log(JSON.stringify(out))
process.exit(0)
"#,
        root = sessions.display().to_string()
    );
    let file = dsh_dir().join(format!("read-logs-{}.mjs", std::process::id()));
    std::fs::write(&file, script).unwrap();
    let output = Command::new("node")
        .arg(&file)
        .current_dir(dsh_dir())
        .output()
        .unwrap();
    std::fs::remove_file(&file).ok();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    serde_json::from_str(stdout.lines().last().unwrap_or_default())
        .unwrap_or_else(|_| panic!("{stdout}"))
}

/// The runs a top-level run delegates to, at any depth, join its DSH turn:
/// here a grandchild's DSH call, under a child that makes none (every run
/// tells the host who delegated it), comes before the parent's own; none of
/// them closes the turn or opens another, and the parent's finish ends it.
#[tokio::test(flavor = "multi_thread")]
async fn delegated_runs_join_the_top_level_run_s_dsh_turn() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let session = fake_session();
    let host = stand_in_host(
        &runtime,
        dir.path(),
        session.clone(),
        Arc::new(ScriptedUi::default()),
    )
    .await;
    let (grandchild, grandchild_seen) = recording(vec![
        call("fixture_steer", json!({ "text": "from the grandchild" })),
        text("grandchild done"),
    ]);
    let deep = yoagent::SubAgentTool::from_provider(
        "deep",
        grandchild,
        yoagent::provider::ModelConfig::mock(),
    )
    .with_extension(host.bridge.extension());
    let (child, _) = recording(vec![
        call("deep", json!({ "task": "steer" })),
        text("child done"),
    ]);
    let helper = yoagent::SubAgentTool::from_provider(
        "helper",
        child,
        yoagent::provider::ModelConfig::mock(),
    )
    .with_tools(vec![Arc::new(deep)])
    .with_extension(host.bridge.extension());
    let (parent, _) = agent(vec![
        call("helper", json!({ "task": "delegate" })),
        call("fixture_turns", json!({})),
        text("done"),
    ]);
    let mut parent = parent
        .with_sub_agent(helper)
        .with_extension(host.bridge.extension());
    let (_, results) = run(&mut parent, "go").await;
    // The grandchild's call had an agent: its steering reached the host.
    assert_eq!(grandchild_seen.lock().unwrap().len(), 2);
    tokio::time::timeout(Duration::from_secs(10), async {
        while session.woken.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the grandchild's steering reaches the host");
    // The parent's call sees the one turn the grandchild's call opened.
    assert_eq!(turns(&results[1].1), (1, vec![]), "{results:?}");
    // The next run finds it ended `completed` by the parent's finish.
    let mut next = agent_again(&host, vec![call("fixture_turns", json!({})), text("done")]);
    let (_, results) = run(&mut next, "go").await;
    assert_eq!(
        turns(&results[0].1),
        (2, vec!["completed".into()]),
        "{results:?}"
    );
    host.root.shutdown().await.unwrap();
}

/// A host whose workspace is not an absolute path gets no stand-in: the
/// call fails, saying why.
#[tokio::test(flavor = "multi_thread")]
async fn a_stand_in_needs_an_absolute_workspace() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("dsh-home");
    std::fs::create_dir_all(&home).unwrap();
    let host = host_serving(
        &runtime,
        None,
        &[
            ("DSH_HOME", &home),
            ("DSH_AGENTS_HOME", &home),
            ("HOME", &home),
        ],
        vec![
            ("host_session", fake_session() as Arc<dyn HostDispatch>),
            (
                "host_workspace",
                Arc::new(FakeWorkspace(PathBuf::from("relative/dir"))),
            ),
        ],
    )
    .await;
    host.load(stand_in_rows(&dir.path().join("sessions"))).await;
    let mut agent = agent_again(&host, vec![call("fixture_where", json!({})), text("done")]);
    let (_, results) = run(&mut agent, "go").await;
    assert!(
        results[0].2 && results[0].1.contains("not an absolute path"),
        "{results:?}"
    );
    host.root.shutdown().await.unwrap();
}

/// A fresh agent over the same host, scripted with `responses`.
fn agent_again(host: &Host, responses: Vec<MockResponse>) -> yoagent::Agent {
    agent(responses).0.with_extension(host.bridge.extension())
}

/// yobuild's 2026-10-19 probe, as a test to re-run on each DSH bump:
/// dsh-univer-office (a DSH plugin whose every file tool needs a calling
/// agent) through the stand-in. It creates a book and a worktree, imports a
/// CSV, merges after the user approves (DSH's approval service, inside the
/// run's turn), and exports a valid .xlsx. Installs dsh-univer-office into a
/// copy of `plugins/dsh` (network; minutes), so `--ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "installs dsh-univer-office from npm; run with --ignored"]
async fn dsh_univer_office_works_through_the_stand_in_agent() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("dsh");
    std::fs::create_dir_all(&base).unwrap();
    for entry in std::fs::read_dir(dsh_dir()).unwrap().flatten() {
        if entry.path().is_file() {
            std::fs::copy(entry.path(), base.join(entry.file_name())).unwrap();
        }
    }
    for args in [
        &["ci", "--no-audit", "--no-fund"][..],
        &[
            "install",
            "--no-save",
            "--no-audit",
            "--no-fund",
            "dsh-univer-office@0.3.7",
        ][..],
    ] {
        let status = Command::new("npm")
            .args(args)
            .current_dir(&base)
            .status()
            .unwrap();
        assert!(status.success(), "npm {args:?}");
    }
    let work = dir.path().join("work");
    let home = dir.path().join("dsh-home");
    for d in [&work, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(work.join("data.csv"), "name,qty\napple,3\npear,5\n").unwrap();
    let session = Arc::new(FakeSession {
        generation: AtomicU64::new(1),
        woken: Mutex::new(Vec::new()),
    });
    let ui = Arc::new(ScriptedUi {
        frontends: 1,
        ..ScriptedUi::default()
    });
    ui.answers.lock().unwrap().push_back(json!("Allow once"));
    let host = host_in(
        &base,
        &runtime,
        Some(ui.clone()),
        &[("DSH_HOME", &home)],
        vec![
            ("host_session", session.clone() as Arc<dyn HostDispatch>),
            ("host_workspace", Arc::new(FakeWorkspace(work.clone()))),
        ],
    )
    .await;
    let mut rows = vec![
        json!({ "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
        json!({ "id": "tools", "name": "@deepseek-ai/dsh-tools" }),
    ];
    rows.extend(
        yoagent_rutis::adapters::dsh_host_rows(&base, &dir.path().join("sessions")).unwrap(),
    );
    // A free port for univer's gateway.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    rows.extend([
        json!({ "id": "univer", "name": "dsh-univer-office", "config": { "telemetry": false, "gatewayPort": port, "gatewayStartupTimeoutMs": 30000 } }),
        json!({ "id": "adapter", "name": base.join("dsh-tools-adapter.ts") }),
        json!({ "id": "dialogs", "name": base.join("host-dialogs.ts") }),
    ]);
    host.load(Value::Array(rows)).await;

    let step = |responses: Vec<MockResponse>| agent_again(&host, responses);
    let book = json!("book.univer");
    let (_, results) = run(
        &mut step(vec![
            call("univer_new", json!({ "file": book })),
            call(
                "univer_worktree",
                json!({ "action": "create", "file": book, "name": "t" }),
            ),
            text("done"),
        ]),
        "go",
    )
    .await;
    assert!(!results[0].2 && !results[1].2, "{results:?}");
    let worktree = Value::from(
        serde_json::from_str::<Value>(&results[1].1).unwrap()["result"]["worktreeId"]
            .as_str()
            .unwrap(),
    );
    let (_, results) = run(
        &mut step(vec![
            call("univer_import", json!({ "source": "data.csv", "file": book, "worktreeId": worktree, "name": "Data" })),
            call("univer_status", json!({ "file": book, "worktreeId": worktree })),
            text("done"),
        ]),
        "go",
    )
    .await;
    assert!(!results[0].2, "{results:?}");
    let unit = serde_json::from_str::<Value>(&results[1].1).unwrap()["result"]["selectedWorktree"]
        ["units"][0]["unitId"]
        .clone();
    assert!(unit.is_string(), "{results:?}");
    let (_, results) = run(
        &mut step(vec![
            call("univer_export", json!({ "file": book, "unitId": unit, "worktreeId": worktree, "output": "out.xlsx" })),
            call("univer_worktree", json!({ "action": "ready", "file": book, "worktreeId": worktree })),
            text("next"),
        ]),
        "go",
    )
    .await;
    assert!(!results[0].2 && !results[1].2, "{results:?}");
    let xlsx = std::fs::read(work.join("out.xlsx")).unwrap();
    assert!(
        xlsx.starts_with(b"PK"),
        "a zip (xlsx), {} bytes",
        xlsx.len()
    );
    // Merging asks: the user's yes, through DSH's approval service.
    let (_, results) = run(
        &mut step(vec![
            call(
                "univer_worktree",
                json!({ "action": "merge", "file": book, "worktreeId": worktree }),
            ),
            text("done"),
        ]),
        "go",
    )
    .await;
    assert!(!results[0].2, "{results:?}");
    assert_eq!(
        ui.asked.lock().unwrap().len(),
        1,
        "the merge was asked about"
    );
    host.root.shutdown().await.unwrap();
}
