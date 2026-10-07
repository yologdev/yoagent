//! TypeScript and Python plugins, end to end: real Node and Python runtimes
//! (rutis-bridge 0.7), loaded as rows by rutis-loader, registering handlers
//! through the `yoagent` host service.
//!
//! Needs Node 24+ with `npm ci` run in `plugins/` (for `@arcships/rutis` and
//! `@arcships/rutis-runtime` 0.7.0), and Python 3.12+ with `rutis` 0.7 —
//! `plugins/.venv/bin/python` (`uv venv plugins/.venv --python 3.12 && uv pip
//! install --python plugins/.venv/bin/python rutis==0.7.0`), or the
//! interpreter in `YOAGENT_RUTIS_PYTHON`. A test whose runtime is missing
//! prints why and passes; set `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1` (CI does) to
//! make a missing runtime fail instead.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
use rutis_bridge::session::{host_key, HostDispatch, Reply as RpcReply, Value as RpcValue};
use rutis_loader::{
    Chain, Layer, Loader, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin,
    ServiceCatalog,
};
use serde_json::{json, Value};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::AgentEvent;
use yoagent_rutis::RutisBridge;

fn plugins_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins")
}

fn required() -> bool {
    std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1")
}

/// A runtime cannot run here: say why and skip (or fail, when required).
fn skip<T>(what: &str, why: String) -> Option<T> {
    if required() {
        panic!("{what} is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1) but unavailable: {why}");
    }
    eprintln!("SKIPPED: {what} unavailable: {why}");
    None
}

/// The Node runtime package, if Node 24+ and `npm ci` in `plugins/` are there.
fn node_runtime() -> Option<PathBuf> {
    let runtime = plugins_dir().join("node_modules/@arcships/rutis-runtime");
    let version = Command::new("node").arg("--version").output();
    let major = match &version {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .trim()
            .trim_start_matches('v')
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok()),
        _ => None,
    };
    match major {
        Some(major) if major >= 24 => {}
        Some(major) => return skip("Node", format!("Node {major} found, 24+ needed")),
        None => return skip("Node", "no `node` on PATH".into()),
    }
    if !runtime.join("package.json").exists() {
        return skip(
            "Node",
            format!("{} missing: run `npm ci` in plugins/", runtime.display()),
        );
    }
    Some(runtime)
}

/// A Python 3.12+ interpreter with rutis 0.7.
fn python() -> Option<PathBuf> {
    let python = std::env::var_os("YOAGENT_RUTIS_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| plugins_dir().join(".venv/bin/python"));
    let probe = Command::new(&python)
        .args([
            "-c",
            "import sys, importlib.metadata as m; \
             print(sys.version_info >= (3, 12), m.version('rutis'))",
        ])
        .output();
    match probe {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            match text.split_once(' ') {
                Some(("True", version)) if version.starts_with("0.7") => Some(python),
                _ => skip(
                    "Python",
                    format!(
                        "{} reports `{text}`: Python 3.12+ and rutis 0.7 needed",
                        python.display()
                    ),
                ),
            }
        }
        _ => skip(
            "Python",
            format!(
                "{} cannot import rutis (set YOAGENT_RUTIS_PYTHON, or create plugins/.venv)",
                python.display()
            ),
        ),
    }
}

/// A host service plugins report to: `probe.record(line)`.
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<String>>>);

impl HostDispatch for Probe {
    fn invoke(&self, method: &str, args: RpcValue) -> RpcReply {
        assert_eq!(method, "record");
        let [line]: [String; 1] = rutis_bridge::session::decode_value(args)?;
        self.0.lock().unwrap().push(line);
        Ok(RpcValue::Undefined)
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "record": "sync" }))
    }
}

impl Probe {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    async fn wait_for(&self, line: &str) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !self.lines().iter().any(|l| l == line) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{line:?} not recorded: {:?}", self.lines()));
    }
}

/// One rutis root with the bridge, a loader, and the runtimes asked for.
struct Host {
    root: Ctx,
    bridge: RutisBridge,
    loader: Loader,
    probe: Probe,
}

async fn host(node: Option<&Path>, python: Option<(&Path, &Path)>) -> Host {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let probe = Probe::default();
    root.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe.clone()))
        .unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    catalog.register_shared("probe");
    let mut chain = Chain::new();
    let mut rows = Vec::new();
    if let Some((modules, interpreter)) = python {
        let runtime = LocalRuntime::python(modules).interpreter(interpreter);
        let resolver = Arc::new(RuntimeResolver::modules(runtime.handle()).with_catalog(&catalog));
        root.plugin(runtime);
        chain = chain.with_shared(resolver.clone());
        rows.push(resolver);
    }
    if let Some(runtime_dir) = node {
        let runtime = LocalRuntime::node(runtime_dir, plugins_dir().join("package.json"));
        let resolver = Arc::new(RuntimeResolver::node(runtime.handle()).with_catalog(&catalog));
        root.plugin(runtime);
        chain = chain.with_shared(resolver.clone());
        rows.push(resolver);
    }
    let plugin = LoaderPlugin::new(
        chain,
        LoaderOptions {
            catalog,
            ..LoaderOptions::default()
        },
    );
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    for resolver in rows {
        root.plugin(RuntimeRowsPlugin::new(resolver));
    }
    Host {
        root,
        bridge,
        loader,
        probe,
    }
}

impl Host {
    async fn load(&self, rows: Vec<Value>) {
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
        let report = self
            .loader
            .reconcile(vec![Layer::new("rows", patches)], None)
            .await
            .unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
    }

    /// Wait until the handlers named are registered (their rows ran `apply`).
    async fn until_handlers(&self, names: &[&str]) {
        let registry = self.bridge.registry().clone();
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let have: Vec<String> = registry
                    .handlers()
                    .iter()
                    .map(|h| h.name().to_string())
                    .collect();
                if names.iter().all(|n| have.contains(n)) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "handlers {names:?} not registered; have {:?}",
                registry.handlers()
            )
        });
    }

    async fn until_no_handler(&self, name: &str) {
        let registry = self.bridge.registry().clone();
        tokio::time::timeout(Duration::from_secs(30), async {
            while registry.handlers().iter().any(|h| h.name() == name) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("handler {name} still registered"));
    }
}

fn ts_row(id: &str, file: &Path, config: Value) -> Value {
    json!({ "id": id, "name": file.to_string_lossy(), "config": config })
}

fn py_row(id: &str, module: &str, config: Value) -> Value {
    json!({ "id": id, "name": format!("py:{module}"), "config": config })
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

/// Leaks a key, for the redactors.
fn env_tool() -> Reply {
    Reply::new("env", "OPENAI_KEY= sk-live-123 HOME=/home/me")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_typescript_and_python_examples_offer_tools_deny_and_redact() {
    let (Some(node), Some(python)) = (node_runtime(), python()) else {
        return;
    };
    let host = host(Some(&node), Some((&plugins_dir().join("python"), &python))).await;
    host.load(vec![
        ts_row("ts", &plugins_dir().join("ts/example.ts"), json!({})),
        py_row("py", "yoagent_example", json!({})),
    ])
    .await;
    host.until_handlers(&["ts-example", "py-example"]).await;

    let bash = Reply::new("bash", "ran bash");
    let bash_runs = bash.runs();
    let (agent, seen) = agent(vec![
        calls(&[
            ("ts_word_count", json!({"text": "one two three"})),
            ("py_reverse", json!({"text": "abc"})),
            ("bash", json!({})),
            ("rm", json!({})),
            ("env", json!({})),
        ]),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![
            Box::new(bash),
            Box::new(Reply::new("rm", "ran rm")),
            Box::new(env_tool()),
        ])
        .with_extension(host.bridge.extension().filters_tool_output());
    let (events, results) = tokio::time::timeout(Duration::from_secs(60), run(&mut agent, "go"))
        .await
        .expect("the run finishes");

    let offered = seen.lock().unwrap()[0].tools.clone();
    for tool in ["ts_word_count", "py_reverse"] {
        assert!(offered.contains(&tool.to_string()), "{offered:?}");
    }
    let by_name = |name: &str| {
        results
            .iter()
            .find(|(n, ..)| n == name)
            .unwrap_or_else(|| panic!("no result for {name}: {results:?}"))
            .clone()
    };
    assert_eq!(
        by_name("ts_word_count"),
        ("ts_word_count".into(), "3 words".into(), false)
    );
    assert_eq!(
        by_name("py_reverse"),
        ("py_reverse".into(), "cba".into(), false)
    );
    let (_, bash_out, bash_err) = by_name("bash");
    assert!(
        bash_err && bash_out.contains("disabled by the ts-example plugin"),
        "{bash_out}"
    );
    assert_eq!(bash_runs.load(Ordering::SeqCst), 0);
    let (_, rm_out, rm_err) = by_name("rm");
    assert!(
        rm_err && rm_out.contains("disabled by the py-example plugin"),
        "{rm_out}"
    );
    let (_, env_out, env_err) = by_name("env");
    assert!(!env_err);
    assert_eq!(env_out, "OPENAI_KEY= [key] HOME=/home/me");
    assert!(
        !format!("{events:?}").contains("sk-live-123"),
        "the key reaches no event"
    );
    // The word count's details crossed as data.
    let details = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionEnd {
            tool_name, result, ..
        } if tool_name == "ts_word_count" => Some(result.details.clone()),
        _ => None,
    });
    assert_eq!(details, Some(json!({"words": 3})));
    host.root.shutdown().await.unwrap();
}

// ── Fixture plugins, written to a temporary directory ───────────

/// Every hook, in JavaScript (the Node runtime runs `.mjs` and `.ts` alike).
const JS_HOOKS: &str = r#"
import { definePlugin } from 'RUTIS'
export default definePlugin({
  inject: ['yoagent', 'probe'],
  apply(ctx, config) {
    const probe = ctx.use('probe')
    const yoagent = ctx.use('yoagent')
    const name = config.name ?? 'js-hooks'
    ctx.effect(yoagent.register(name, {
      async before_tool(call) {
        if (call.tool === 'echo_args' && call.args.path) {
          return { args: { ...call.args, path: '/sandbox' + call.args.path } }
        }
        if (call.tool === 'fail_policy') throw new Error('policy backend down')
        if (call.tool === 'slow_policy') await new Promise(resolve => setTimeout(resolve, 60000))
        if (call.tool === 'odd_policy') return { allow: 'maybe' }
      },
      async before_model(turn) {
        if (turn.user_request?.includes('stop now')) return { stop: 'js says stop' }
        return `[js note: ${turn.tools.join(',')} depth ${turn.depth} label ${turn.label}]`
      },
      async on_input(input) {
        if (input.text.includes('forbidden')) return { reject: 'js rejects forbidden input' }
      },
      async on_stop(stop) {
        if (!stop.answer.includes('verified')) return { continue: 'verify first' }
      },
      async finish(outcome) {
        probe.record(`js finish: ${outcome.end} ${outcome.label}`)
      },
      async on_event(event) {
        probe.record(`js event: ${event.type} ${event.run.label}`)
      },
    }, { events: ['toolExecutionEnd', 'agentEnd'] }))
    probe.record(`js ${name}: registered`)
  },
})
"#;

/// A dict of functions, in Python, and a tool that kills its runtime.
const PY_HOOKS: &str = r#"
import os

inject = ["yoagent", "probe"]


def apply(ctx, config):
    probe = ctx.use("probe")
    yoagent = ctx.use("yoagent")

    async def on_input(input):
        if "python-forbidden" in input["text"]:
            return {"reject": "py rejects"}
        return None

    async def before_tool(call):
        if call["tool"] == "py_denied":
            return {"deny": "py denies"}
        return None

    async def after_tool(call, output):
        if call["tool"] == "env":
            return {"text": output["text"].upper(), "details": {"py": True}}
        return None

    async def tools(run):
        return [{"name": "py_crash", "description": "exits the Python runtime"}]

    async def call_tool(call):
        os._exit(3)

    async def finish(outcome):
        probe.record("py finish: " + outcome["end"])

    name = (config or {}).get("name", "py-hooks")
    ctx.effect(yoagent.register(name, {
        "on_input": on_input,
        "before_tool": before_tool,
        "after_tool": after_tool,
        "tools": tools,
        "call_tool": call_tool,
        "finish": finish,
    }))
    probe.record(f"py {name}: registered")
"#;

/// Registrations `register` must refuse, each answer recorded; then a
/// handler whose `on_event` throws and whose `call_tool` answers badly.
const JS_STRICT: &str = r#"
import { definePlugin } from 'RUTIS'
export default definePlugin({
  inject: ['yoagent', 'probe'],
  apply(ctx) {
    const probe = ctx.use('probe')
    const yoagent = ctx.use('yoagent')
    const attempts = {
      'no hooks': [{}],
      'tools without call_tool': [{ async tools() { return [] } }],
      'events without on_event': [{ async before_tool() {} }, { events: ['agentEnd'] }],
      'on_event without events': [{ async on_event() {} }],
      'a hook that is not a function': [{ before_tool: 42, async on_stop() {} }],
    }
    for (const [what, args] of Object.entries(attempts)) {
      try {
        yoagent.register('bad', ...args)
        probe.record(`accepted: ${what}`)
      } catch (error) {
        probe.record(`refused: ${what}: ${error.message}`)
      }
    }
    ctx.effect(yoagent.register('strict', {
      async tools() {
        return [{ name: 'bad_flag' }, { name: 'bad_text' }, { name: 'empty' }]
      },
      async call_tool(call) {
        if (call.tool === 'bad_flag') return { text: 'ok?', is_error: 'yes' }
        if (call.tool === 'bad_text') return { text: 42 }
        return {}
      },
      async before_tool(call) {
        if (call.tool === 'bad_args') return { args: 'not an object' }
      },
      async on_event(event) {
        throw new Error('observer down')
      },
    }, { events: ['toolExecutionEnd'] }))
  },
})
"#;

struct Fixtures {
    js: PathBuf,
    strict: PathBuf,
    py: PathBuf,
    _dir: tempfile::TempDir,
}

fn fixtures() -> Fixtures {
    let dir = tempfile::tempdir().unwrap();
    let rutis = plugins_dir()
        .join("node_modules/@arcships/rutis/src/index.mjs")
        .canonicalize()
        .map(|p| format!("file://{}", p.display()))
        .unwrap_or_default();
    let js = dir.path().join("hooks.mjs");
    std::fs::write(&js, JS_HOOKS.replace("RUTIS", &rutis)).unwrap();
    let strict = dir.path().join("strict.mjs");
    std::fs::write(&strict, JS_STRICT.replace("RUTIS", &rutis)).unwrap();
    let py = dir.path().join("py");
    std::fs::create_dir_all(&py).unwrap();
    std::fs::write(py.join("py_hooks.py"), PY_HOOKS).unwrap();
    Fixtures {
        js,
        strict,
        py,
        _dir: dir,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typescript_handler_gets_every_hook() {
    let Some(node) = node_runtime() else {
        return;
    };
    let fx = fixtures();
    let host = host(Some(&node), None).await;
    host.load(vec![ts_row("hooks", &fx.js, json!({}))]).await;
    host.until_handlers(&["js-hooks"]).await;

    let (agent, seen) = agent(vec![
        call("echo_args", json!({"path": "/etc"})),
        text("done"),
        text("done, verified"),
    ]);
    let mut agent = agent
        .with_run_label("lbl")
        .with_tools(vec![Box::new(EchoArgs)])
        .with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "please act").await;
    // before_tool rewrote the arguments.
    assert_eq!(
        results,
        vec![(
            "echo_args".into(),
            r#"{"path":"/sandbox/etc"}"#.into(),
            false
        )]
    );
    let seen_now = seen.lock().unwrap().clone();
    // before_model added a note to every request, never stored.
    assert!(
        seen_now[0]
            .last_user
            .ends_with("[js note: echo_args depth 0 label lbl]"),
        "{:?}",
        seen_now[0]
    );
    assert!(!format!("{:?}", agent.messages()).contains("js note"));
    // on_stop sent the model back once.
    assert_eq!(seen_now.len(), 3, "{seen_now:?}");
    assert!(
        seen_now[2].last_user.contains("verify first"),
        "{seen_now:?}"
    );
    // on_event got only the subscribed types, in order; every event sent
    // before `finish` arrived before it (yoagent sends `agentEnd` after).
    host.probe.wait_for("js event: agentEnd lbl").await;
    let lines: Vec<String> = host
        .probe
        .lines()
        .into_iter()
        .filter(|l| l.starts_with("js event") || l.starts_with("js finish"))
        .collect();
    assert_eq!(
        lines,
        vec![
            "js event: toolExecutionEnd lbl",
            "js finish: completed lbl",
            "js event: agentEnd lbl",
        ]
    );

    // on_input rejects.
    let (events, _) = run(&mut agent, "something forbidden").await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRejected { reason } if reason == "js rejects forbidden input"
        )),
        "{events:?}"
    );
    // before_model stops the run before any request.
    let before = seen.lock().unwrap().len();
    run(&mut agent, "stop now").await;
    assert_eq!(seen.lock().unwrap().len(), before);
    assert!(format!("{:?}", agent.messages()).contains("js says stop"));
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_throwing_slow_or_malformed_language_policy_denies() {
    let Some(node) = node_runtime() else {
        return;
    };
    let fx = fixtures();
    let host = host(Some(&node), None).await;
    host.load(vec![ts_row("hooks", &fx.js, json!({}))]).await;
    host.until_handlers(&["js-hooks"]).await;
    let tools: Vec<Reply> = ["fail_policy", "slow_policy", "odd_policy"]
        .iter()
        .map(|n| Reply::new(n, "ran"))
        .collect();
    let runs: Vec<_> = tools.iter().map(Reply::runs).collect();
    let (agent, _) = agent(vec![
        calls(&[
            ("fail_policy", json!({})),
            ("slow_policy", json!({})),
            ("odd_policy", json!({})),
        ]),
        text("done, verified"),
    ]);
    let mut agent = agent
        .with_tools(
            tools
                .into_iter()
                .map(|t| Box::new(t) as Box<dyn yoagent::AgentTool>)
                .collect(),
        )
        .with_extension(
            host.bridge
                .extension()
                .with_policy_timeout(Some(Duration::from_millis(500))),
        );
    let (_, results) = tokio::time::timeout(Duration::from_secs(30), run(&mut agent, "go"))
        .await
        .expect("the timeout bounds the slow policy");
    assert!(
        runs.iter().all(|r| r.load(Ordering::SeqCst) == 0),
        "{results:?}"
    );
    let reason = |name: &str| {
        let (_, text, is_error) = results.iter().find(|(n, ..)| n == name).unwrap().clone();
        assert!(is_error, "{name} denied");
        text
    };
    assert!(
        reason("fail_policy").contains("policy backend down"),
        "{results:?}"
    );
    assert!(
        reason("slow_policy").contains("did not answer"),
        "{results:?}"
    );
    assert!(
        reason("odd_policy").contains("unexpected value"),
        "{results:?}"
    );
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_python_dict_of_functions_is_a_handler() {
    let Some(python) = python() else {
        return;
    };
    let fx = fixtures();
    let host = host(None, Some((&fx.py, &python))).await;
    host.load(vec![py_row("py", "py_hooks", json!({}))]).await;
    host.until_handlers(&["py-hooks"]).await;

    let (agent, seen) = agent(vec![
        calls(&[("py_denied", json!({})), ("env", json!({}))]),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![
            Box::new(Reply::new("py_denied", "ran")),
            Box::new(env_tool()),
        ])
        .with_extension(host.bridge.extension());
    let (events, results) = run(&mut agent, "go").await;
    assert!(seen.lock().unwrap()[0]
        .tools
        .contains(&"py_crash".to_string()));
    assert_eq!(
        results,
        vec![
            (
                "py_denied".into(),
                "Tool call denied: py denies".into(),
                true
            ),
            (
                "env".into(),
                "OPENAI_KEY= SK-LIVE-123 HOME=/HOME/ME".into(),
                false
            ),
        ]
    );
    let details = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionEnd {
            tool_name, result, ..
        } if tool_name == "env" => Some(result.details.clone()),
        _ => None,
    });
    assert_eq!(details, Some(json!({"py": true})));
    host.probe.wait_for("py finish: completed").await;

    let (events, _) = run(&mut agent, "python-forbidden").await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::InputRejected { reason } if reason == "py rejects"
    )));
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unloading_a_row_unregisters_its_handler() {
    let Some(node) = node_runtime() else {
        return;
    };
    let host = host(Some(&node), None).await;
    host.load(vec![ts_row(
        "ts",
        &plugins_dir().join("ts/example.ts"),
        json!({}),
    )])
    .await;
    host.until_handlers(&["ts-example"]).await;

    // `ctx.effect(unregister)` runs when the loader removes the row.
    let report = host.loader.reconcile(vec![], None).await.unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    host.until_no_handler("ts-example").await;

    let bash = Reply::new("bash", "ran bash");
    let bash_runs = bash.runs();
    let (agent, seen) = agent(vec![call("bash", json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(bash)])
        .with_extension(host.bridge.extension());
    run(&mut agent, "go").await;
    assert_eq!(seen.lock().unwrap()[0].tools, vec!["bash".to_string()]);
    assert_eq!(bash_runs.load(Ordering::SeqCst), 1, "no policy any more");
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_runtime_withdraws_its_handlers() {
    let Some(python) = python() else {
        return;
    };
    let fx = fixtures();
    let host = host(None, Some((&fx.py, &python))).await;
    host.load(vec![py_row("py", "py_hooks", json!({}))]).await;
    host.until_handlers(&["py-hooks"]).await;

    let denied = Reply::new("py_denied", "ran");
    let denied_runs = denied.runs();
    let (agent, _) = agent(vec![
        call("py_crash", json!({})),
        call("py_denied", json!({})),
        text("done"),
        call("py_denied", json!({})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(denied)])
        .with_extension(host.bridge.extension());
    let (_, results) = tokio::time::timeout(Duration::from_secs(30), run(&mut agent, "go"))
        .await
        .expect("the crash ends the call");
    // The call that killed the runtime fails; the rest of the run holds a
    // handler that is gone, so its policy denies.
    assert!(results[0].2, "{results:?}");
    assert!(results[1].2, "{results:?}");
    // Denied either way: as unavailable once the closed session was
    // noticed, or by the failed call before that.
    assert!(
        results[1].1.contains("no longer available") || results[1].1.contains("py-hooks"),
        "{results:?}"
    );
    host.until_no_handler("py-hooks").await;

    // The next run has no Python handler at all.
    let (_, results) = run(&mut agent, "again").await;
    assert_eq!(results, vec![("py_denied".into(), "ran".into(), false)]);
    assert_eq!(denied_runs.load(Ordering::SeqCst), 1);
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn handler_names_are_unique_across_languages() {
    let (Some(node), Some(python)) = (node_runtime(), python()) else {
        return;
    };
    let fx = fixtures();
    let host = host(Some(&node), Some((&fx.py, &python))).await;
    // A Rust plugin holds the name first.
    let rust = host.root.plugin(plugin(handler("taken")));
    wait_active(&rust).await;
    let rows = vec![
        ts_row("js", &fx.js, json!({"name": "taken"})),
        py_row("py", "py_hooks", json!({"name": "taken"})),
    ];
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
    let report = host
        .loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();
    // `register` threw in both languages, so both rows failed to load.
    let failed: Vec<(&str, &str)> = report
        .failures
        .iter()
        .map(|f| (f.id.as_str(), f.error.as_str()))
        .collect();
    assert_eq!(failed.len(), 2, "{report:?}");
    for (id, error) in failed {
        assert!(["js", "py"].contains(&id));
        assert!(
            error.contains("yoagent handler `taken`") && error.contains("already registered"),
            "{error}"
        );
    }
    let handlers = host.bridge.registry().handlers();
    assert_eq!(handlers.len(), 1, "{handlers:?}");
    assert!(!host.probe.lines().iter().any(|l| l.contains("registered")));
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tree_extension_carries_a_language_policy_into_sub_agents() {
    let Some(node) = node_runtime() else {
        return;
    };
    let fx = fixtures();
    let host = host(Some(&node), None).await;
    host.load(vec![ts_row("hooks", &fx.js, json!({}))]).await;
    host.until_handlers(&["js-hooks"]).await;

    let (child, child_seen) = recording(vec![
        call("echo_args", json!({"path": "/etc"})),
        text("child done, verified"),
    ]);
    let sub = yoagent::SubAgentTool::from_provider(
        "helper",
        child,
        yoagent::provider::ModelConfig::mock(),
    )
    .with_tools(vec![Arc::new(EchoArgs)]);
    let (parent, _) = agent(vec![
        call("helper", json!({"task": "echo"})),
        text("done, verified"),
    ]);
    let mut parent = parent
        .with_run_label("tree")
        .with_sub_agent(sub)
        .with_tree_extension(host.bridge.extension());
    let (events, _) = run(&mut parent, "delegate").await;

    // The child's request carried the plugin's note, at depth 1...
    let child_seen = child_seen.lock().unwrap().clone();
    assert!(
        child_seen[0]
            .last_user
            .ends_with("[js note: echo_args depth 1 label tree]"),
        "{child_seen:?}"
    );
    // ...and the delegation completed under the plugin's hooks.
    let child_result = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionEnd {
            tool_name, result, ..
        } if tool_name == "helper" => Some(result_text(result)),
        _ => None,
    });
    let child_result = child_result.expect("the delegation ended");
    assert!(
        child_result.contains("child done"),
        "the delegation succeeded: {child_result}"
    );
    host.probe.wait_for("js event: toolExecutionEnd tree").await;
    assert!(
        format!("{:?}", parent.messages()).contains("helper"),
        "the parent ran the delegation"
    );
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn register_and_results_are_strict() {
    let Some(node) = node_runtime() else {
        return;
    };
    let fx = fixtures();
    let host = host(Some(&node), None).await;
    host.load(vec![ts_row("strict", &fx.strict, json!({}))])
        .await;
    host.until_handlers(&["strict"]).await;
    for (what, why) in [
        ("no hooks", "must be an object (or dict) of async functions"),
        ("tools without call_tool", "must implement `call_tool`"),
        ("events without on_event", "needs an `on_event` method"),
        ("on_event without events", "needs `options.events`"),
        (
            "a hook that is not a function",
            "`before_tool` is not a function",
        ),
    ] {
        let line = host
            .probe
            .lines()
            .into_iter()
            .find(|l| l.contains(what))
            .unwrap_or_else(|| panic!("{what}: {:?}", host.probe.lines()));
        assert!(line.starts_with("refused") && line.contains(why), "{line}");
    }

    for required in [false, true] {
        let echo = Reply::new("bad_args", "ran");
        let echo_runs = echo.runs();
        let (agent, _) = agent(vec![
            calls(&[
                ("bad_flag", json!({})),
                ("bad_text", json!({})),
                ("empty", json!({})),
                ("bad_args", json!({})),
                ("pause", json!({})),
            ]),
            text("done"),
        ]);
        let extension = if required {
            host.bridge.extension().required()
        } else {
            host.bridge.extension()
        };
        // A remote `on_event` failure is noticed at the first event or
        // decision point after it lands: the pause lets it land before the
        // next model request.
        let mut agent = agent
            .with_tools(vec![Box::new(echo), Box::new(Pause)])
            .with_extension(extension);
        let (_, results) = tokio::time::timeout(Duration::from_secs(30), run(&mut agent, "go"))
            .await
            .expect("the run finishes");
        if required {
            // The failed `on_event` (after the first tool's end) fails the
            // run; calls not started yet do not run.
            let error = run_error(&agent).expect("the run failed");
            assert!(error.contains("observer down"), "{error}");
            continue;
        }
        let result = |name: &str| {
            results
                .iter()
                .find(|(n, ..)| n == name)
                .unwrap_or_else(|| panic!("{name}: {results:?}"))
                .clone()
        };
        for name in ["bad_flag", "bad_text", "empty"] {
            let (_, text, is_error) = result(name);
            assert!(
                is_error && text.contains("unexpected value"),
                "{name}: {text}"
            );
        }
        let (_, text, is_error) = result("bad_args");
        assert!(is_error && text.contains("unexpected value"), "{text}");
        assert_eq!(echo_runs.load(Ordering::SeqCst), 0);
        assert_eq!(run_error(&agent), None, "advisory: on_event switched off");
    }
    host.root.shutdown().await.unwrap();
}

/// Sleeps briefly.
struct Pause;

#[async_trait::async_trait]
impl yoagent::AgentTool for Pause {
    fn name(&self) -> &str {
        "pause"
    }
    fn label(&self) -> &str {
        "pause"
    }
    fn description(&self) -> &str {
        "waits half a second"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: Value,
        _ctx: yoagent::ToolContext,
    ) -> Result<yoagent::ToolResult, yoagent::ToolError> {
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(yoagent::ToolResult {
            content: vec![],
            details: Value::Null,
        })
    }
}
