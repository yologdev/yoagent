//! dsh (DeepSeek Harness) tool plugins as yoagent tools, end to end: the
//! adapter `plugins/dsh/dsh-tools-adapter.ts` and a fixture dsh tool plugin
//! (`plugins/dsh/fixture-tools.ts`, no network) loaded with
//! `@deepseek-ai/dsh-system-prompt` and `@deepseek-ai/dsh-tools` as
//! rutis-loader rows of one Node runtime; for dialogs, also
//! `@deepseek-ai/dsh-user-questions`, `@deepseek-ai/dsh-tool-ask-user` and
//! `plugins/dsh/host-dialogs.ts`, against a scripted host `ui` service.
//!
//! Needs Node 24+ and `npm ci` in `plugins/dsh/`. Without them the test
//! prints `SKIPPED:` and passes; `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1` (CI) makes
//! it fail instead.
#![cfg(unix)]

mod common;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
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

/// The Node runtime package, if Node 24+ and `npm ci` in `plugins/dsh/` are there.
fn node_runtime() -> Option<PathBuf> {
    let skip = |why: String| -> Option<PathBuf> {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("the dsh runtime is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1) but unavailable: {why}");
        }
        eprintln!("SKIPPED: the dsh runtime is unavailable: {why}");
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
    let modules = dsh_dir().join("node_modules");
    for package in ["@arcships/rutis-runtime", "@deepseek-ai/dsh-tools"] {
        if !modules.join(package).join("package.json").exists() {
            return skip(format!(
                "{package} missing from {}: run `npm ci` in plugins/dsh/",
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
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    if let Some(ui) = ui {
        catalog.register_shared("ui");
        root.provide_as::<dyn HostDispatch>(host_key("ui"), ui)
            .unwrap();
    }
    let node = LocalRuntime::node(runtime, dsh_dir().join("package.json"));
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
