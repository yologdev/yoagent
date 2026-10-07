//! Each hook a plugin handler can implement, through a real agent run.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use yoagent::extension::{
    ExtensionError, InputDecision, RunEnd, StopDecision, ToolOutput, TurnDecision,
};
use yoagent::{AgentEvent, Content, ToolDecision, ToolResult};

#[tokio::test(flavor = "multi_thread")]
async fn no_policy_handler_allows_the_call() {
    let (root, bridge) = setup();
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(results, vec![("act".into(), "acted".into(), false)]);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_denies_a_call_and_the_model_gets_the_reason() {
    let (root, bridge) = setup();
    let policy = root.plugin(plugin(handler("no-act").with_before_tool(|call| {
        if call.tool == "act" {
            ToolDecision::Deny("act is forbidden by policy".into())
        } else {
            ToolDecision::Allow
        }
    })));
    wait_active(&policy).await;

    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![
        call("act", serde_json::json!({})),
        text("ok"),
        call("act", serde_json::json!({})),
        text("ok"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a denied tool does not run");
    assert!(results[0].2);
    assert!(
        results[0].1.contains("act is forbidden by policy"),
        "{results:?}"
    );

    // Unloading the policy plugin lifts the denial from the next run.
    policy.dispose().await.unwrap();
    let (_, results) = run(&mut agent, "again").await;
    assert_eq!(results, vec![("act".into(), "acted".into(), false)]);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_can_rewrite_arguments_and_later_handlers_see_them() {
    let (root, bridge) = setup();
    let sandbox = root.plugin(plugin(handler("sandbox").with_before_tool(|call| {
        match call.args.get("path").and_then(|p| p.as_str()) {
            Some(path) => {
                let mut args = call.args.clone();
                args["path"] = format!("/sandbox{path}").into();
                ToolDecision::Modify(args)
            }
            None => ToolDecision::Allow,
        }
    })));
    wait_active(&sandbox).await;
    // Registered after: judges the rewritten path.
    let guard =
        root.plugin(plugin(handler("guard").with_before_tool(
            |call| match call.args["path"].as_str() {
                Some(p) if p.starts_with("/sandbox/") => ToolDecision::Allow,
                other => ToolDecision::Deny(format!("unsandboxed path {other:?}")),
            },
        )));
    wait_active(&guard).await;

    let (agent, _) = agent(vec![
        call("echo_args", serde_json::json!({"path": "/etc/passwd"})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(EchoArgs)])
        .with_extension(bridge.extension());
    let (events, results) = run(&mut agent, "go").await;
    assert_eq!(results[0].1, r#"{"path":"/sandbox/etc/passwd"}"#);
    assert!(!results[0].2);
    let started = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionStart { args, .. } => Some(args.clone()),
        _ => None,
    });
    assert_eq!(
        started,
        Some(serde_json::json!({"path": "/sandbox/etc/passwd"})),
        "the tool runs with the rewritten arguments"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_erroring_policy_denies_fail_closed() {
    let (root, bridge) = setup();
    let failing = root.plugin(plugin(handler("failing-policy").with_before_tool_async(
        |_| async { Err(ExtensionError::new("policy backend unreachable")) },
    )));
    wait_active(&failing).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(results[0].2);
    assert!(
        results[0].1.contains("policy backend unreachable")
            && results[0].1.contains("failing-policy"),
        "names the handler and its error: {results:?}"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_policy_denies_fail_closed() {
    let (root, bridge) = setup();
    let panicking = root.plugin(plugin(
        handler("panicky").with_before_tool(|_| panic!("policy bug")),
    ));
    wait_active(&panicking).await;
    let tool = Reply::new("act", "acted");
    let runs = tool.runs();
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(tool)])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(results[0].2);
    assert!(
        results[0].1.contains("plugin handler `panicky` panicked")
            && results[0].1.contains("policy bug"),
        "{results:?}"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_policy_that_times_out_denies() {
    let (root, bridge) = setup();
    let hanging = root.plugin(plugin(handler("hanging-policy").with_before_tool_async(
        |_| async {
            std::future::pending::<()>().await;
            Ok(ToolDecision::Allow)
        },
    )));
    wait_active(&hanging).await;
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_extension(bridge.extension().with_timeout(Duration::from_millis(100)));
    let (_, results) = tokio::time::timeout(Duration::from_secs(5), run(&mut agent, "go"))
        .await
        .expect("the timeout bounds the run");
    assert!(results[0].2);
    assert!(results[0].1.contains("did not answer"), "{results:?}");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_note_reaches_the_request_but_not_history() {
    let (root, bridge) = setup();
    let noter = root.plugin(plugin(
        handler("noter")
            .with_before_model(|turn| TurnDecision::Note(format!("[note for {}]", turn.model)))
            .with_before_model(|turn| match &turn.user_request {
                Some(r) if r.contains("deploy") => {
                    TurnDecision::Note("[deploys need approval]".into())
                }
                _ => TurnDecision::Continue,
            }),
    ));
    wait_active(&noter).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_extension(bridge.extension());
    run(&mut agent, "please deploy").await;
    let last_user = seen.lock().unwrap()[0].last_user.clone();
    assert!(last_user.starts_with("please deploy|"), "{last_user}");
    assert!(last_user.contains("[note for mock]"), "{last_user}");
    assert!(last_user.contains("[deploys need approval]"), "{last_user}");
    let stored = format!("{:?}", agent.messages());
    assert!(!stored.contains("deploys need approval"), "never stored");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_turn_handler_fails_open_and_keeps_the_others_notes() {
    let (root, bridge) = setup();
    let first = root.plugin(plugin(
        handler("first").with_before_model(|_| TurnDecision::Note("[kept]".into())),
    ));
    wait_active(&first).await;
    let second = root.plugin(plugin(
        handler("second").with_before_model(|_| panic!("note bug")),
    ));
    wait_active(&second).await;
    let third = root.plugin(plugin(
        handler("third").with_before_model(|_| TurnDecision::Note("[also kept]".into())),
    ));
    wait_active(&third).await;
    let (agent, seen) = agent(vec![text("done")]);
    let mut agent = agent.with_extension(bridge.extension());
    run(&mut agent, "hi").await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "the request still went out");
    assert_eq!(seen[0].last_user, "hi|[kept]\n[also kept]");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_handler_can_stop_the_run() {
    let (root, bridge) = setup();
    let stopper = root.plugin(plugin(
        handler("stopper").with_before_model(|_| TurnDecision::Stop("quota used up".into())),
    ));
    wait_active(&stopper).await;
    let (agent, seen) = agent(vec![text("never")]);
    let mut agent = agent.with_extension(bridge.extension());
    run(&mut agent, "hi").await;
    assert!(seen.lock().unwrap().is_empty(), "no request was sent");
    let stored = format!("{:?}", agent.messages());
    assert!(stored.contains("quota used up"), "{stored}");
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_input_handler_rejects_a_prompt() {
    let (root, bridge) = setup();
    let filter = root.plugin(plugin(handler("filter").with_on_input(|input| {
        if input.text.contains("rm -rf") {
            InputDecision::Reject("destructive request refused".into())
        } else {
            InputDecision::Pass
        }
    })));
    wait_active(&filter).await;
    let (agent, seen) = agent(vec![text("fine")]);
    let mut agent = agent.with_extension(bridge.extension());

    let (events, _) = run(&mut agent, "please rm -rf /").await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::InputRejected { reason } if reason == "destructive request refused"
    )));
    assert!(seen.lock().unwrap().is_empty(), "the model never saw it");

    let (events, _) = run(&mut agent, "list files").await;
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::InputRejected { .. })));
    assert_eq!(seen.lock().unwrap().len(), 1);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_input_handler_rejects_fail_closed() {
    let (root, bridge) = setup();
    let p = root.plugin(plugin(
        handler("panicking-input").with_on_input(|_| panic!("filter bug")),
    ));
    wait_active(&p).await;
    let (agent, seen) = agent(vec![text("fine")]);
    let mut agent = agent.with_extension(bridge.extension());
    let (events, _) = run(&mut agent, "hello").await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRejected { reason } if reason.contains("input rejected") && reason.contains("plugin handler `panicking-input` panicked")
        )),
        "{events:?}"
    );
    assert!(seen.lock().unwrap().is_empty());
    root.shutdown().await.unwrap();
}

fn redact(output: &mut ToolOutput, secret: &str) {
    for block in &mut output.result.content {
        if let Content::Text { text } = block {
            *text = text.replace(secret, "[redacted]");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn after_tool_redacts_what_the_model_history_and_events_see() {
    let (root, bridge) = setup();
    let redactor = root.plugin(plugin(handler("redactor").with_after_tool(
        |_call, output| {
            redact(output, "hunter2");
            Ok(())
        },
    )));
    wait_active(&redactor).await;
    // A later handler sees the earlier edit.
    let seen_by_later = Arc::new(Mutex::new(Vec::new()));
    let later = root.plugin(plugin(handler("later").with_after_tool({
        let seen_by_later = seen_by_later.clone();
        move |call, output| {
            seen_by_later
                .lock()
                .unwrap()
                .push((call.tool.clone(), result_text(&output.result)));
            Ok(())
        }
    })));
    wait_active(&later).await;

    let (agent, _) = agent(vec![call("read", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("read", "password: hunter2"))])
        .with_extension(bridge.extension().filters_tool_output());
    let (events, results) = run(&mut agent, "go").await;
    assert_eq!(
        results,
        vec![("read".into(), "password: [redacted]".into(), false)]
    );
    let ended = events.iter().find_map(|e| match e {
        AgentEvent::ToolExecutionEnd { result, .. } => Some(result_text(result)),
        _ => None,
    });
    assert_eq!(ended.as_deref(), Some("password: [redacted]"));
    assert!(!format!("{:?}", agent.messages()).contains("hunter2"));
    assert_eq!(
        seen_by_later.lock().unwrap().clone(),
        vec![("read".to_string(), "password: [redacted]".to_string())]
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_after_tool_withholds_the_result() {
    let (root, bridge) = setup();
    let failing = root.plugin(plugin(handler("broken-redactor").with_after_tool_async(
        |_call, _output: ToolOutput| async { Err(ExtensionError::new("redaction service down")) },
    )));
    wait_active(&failing).await;
    let (agent, _) = agent(vec![call("read", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("read", "password: hunter2"))])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert!(results[0].2, "{results:?}");
    assert!(results[0].1.contains("withheld"), "{results:?}");
    assert!(!format!("{:?}", agent.messages()).contains("hunter2"));
    assert_eq!(
        run_error(&agent),
        None,
        "an advisory extension does not fail the run"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_after_tool_fails_a_required_run() {
    let (root, bridge) = setup();
    let failing = root
        .plugin(plugin(handler("broken-redactor").with_after_tool(
            |_, _| Err(ExtensionError::new("redaction service down")),
        )));
    wait_active(&failing).await;
    let (agent, _) = agent(vec![call("read", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("read", "password: hunter2"))])
        .with_extension(bridge.extension().required());
    run(&mut agent, "go").await;
    let error = run_error(&agent).expect("the run failed");
    assert!(
        error.starts_with("[Extension failed: rutis]") && error.contains("broken-redactor"),
        "{error}"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn on_stop_continues_the_run_until_the_verifier_accepts() {
    let (root, bridge) = setup();
    let stops = Arc::new(Mutex::new(Vec::new()));
    let verifier = root.plugin(plugin(handler("verifier").with_on_stop({
        let stops = stops.clone();
        move |stop| {
            stops
                .lock()
                .unwrap()
                .push((stop.answer.clone(), stop.continues));
            if stop.answer.contains("tests pass") {
                StopDecision::Accept
            } else {
                StopDecision::Continue("run the tests before answering".into())
            }
        }
    })));
    wait_active(&verifier).await;
    let (agent, seen) = agent(vec![text("done, I think"), text("done, tests pass")]);
    let mut agent = agent.with_extension(bridge.extension());
    run(&mut agent, "fix the bug").await;
    assert_eq!(
        stops.lock().unwrap().clone(),
        vec![
            ("done, I think".to_string(), 0),
            ("done, tests pass".to_string(), 1)
        ]
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "the verifier sent the model back once");
    assert!(
        seen[1].last_user.contains("run the tests before answering"),
        "{:?}",
        seen[1]
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_on_stop_failure_fails_a_required_run_and_is_skipped_otherwise() {
    for required in [false, true] {
        let (root, bridge) = setup();
        let verifier =
            root.plugin(plugin(handler("strict").with_on_stop(|_| {
                StopDecision::Fail("answer is unverified".into())
            })));
        wait_active(&verifier).await;
        let (agent, _) = agent(vec![text("done")]);
        let extension = if required {
            bridge.extension().required()
        } else {
            bridge.extension()
        };
        let mut agent = agent.with_extension(extension);
        run(&mut agent, "go").await;
        match run_error(&agent) {
            Some(error) => {
                assert!(required, "advisory: skipped, not failed: {error}");
                assert!(error.contains("answer is unverified"), "{error}");
            }
            None => assert!(!required, "a required extension fails the run"),
        }
        root.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn finish_sees_how_every_run_ended() {
    let (root, bridge) = setup();
    let ends = Arc::new(Mutex::new(Vec::new()));
    let auditor = root.plugin(plugin(
        handler("auditor")
            .with_on_input(|input| {
                if input.text == "bad" {
                    InputDecision::Reject("no".into())
                } else {
                    InputDecision::Pass
                }
            })
            .with_finish({
                let ends = ends.clone();
                move |outcome, run| {
                    ends.lock()
                        .unwrap()
                        .push((outcome.end().clone(), run.label.clone()));
                }
            }),
    ));
    wait_active(&auditor).await;
    let (agent, _) = agent(vec![text("done")]);
    let mut agent = agent
        .with_run_label("session-1")
        .with_extension(bridge.extension());
    run(&mut agent, "good").await;
    run(&mut agent, "bad").await;
    let ends = ends.lock().unwrap().clone();
    assert_eq!(ends.len(), 2, "{ends:?}");
    assert_eq!(ends[0], (RunEnd::Completed, Some("session-1".into())));
    assert!(
        matches!(&ends[1].0, RunEnd::Rejected { reason } if reason == "no"),
        "{ends:?}"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rust_handler_observes_every_event_in_order() {
    let (root, bridge) = setup();
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let observer = root.plugin(plugin(handler("observer").with_on_event({
        let kinds = kinds.clone();
        move |_run, event| {
            let kind = serde_json::to_value(event).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string();
            kinds.lock().unwrap().push(kind);
        }
    })));
    wait_active(&observer).await;
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_extension(bridge.extension());
    let (events, _) = run(&mut agent, "go").await;
    let expected: Vec<String> = events
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        *kinds.lock().unwrap(),
        expected,
        "the same events, in order"
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn handlers_get_plain_data_about_the_run() {
    let (root, bridge) = setup();
    type Calls = Arc<Mutex<Vec<yoagent_rutis::ToolCall>>>;
    let calls: Calls = Arc::default();
    let turns: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let counted = Arc::new(AtomicUsize::new(0));
    let observer = root.plugin(plugin(
        handler("observer")
            .with_before_tool({
                let calls = calls.clone();
                move |call| {
                    calls.lock().unwrap().push(call.clone());
                    ToolDecision::Allow
                }
            })
            .with_before_model({
                let turns = turns.clone();
                let counted = counted.clone();
                move |turn| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    turns.lock().unwrap().push(turn.tools.clone());
                    TurnDecision::Continue
                }
            }),
    ));
    wait_active(&observer).await;
    let (agent, _) = agent(vec![call("act", serde_json::json!({"n": 1})), text("done")]);
    let mut agent = agent
        .with_run_label("ui")
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_extension(bridge.extension());
    run(&mut agent, "please act now").await;
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let json = serde_json::to_value(&calls[0]).unwrap();
    assert_eq!(json["tool"], "act");
    assert_eq!(json["args"], serde_json::json!({"n": 1}));
    assert_eq!(json["user_request"], "please act now");
    assert_eq!(json["latest_user_text"], "please act now");
    assert_eq!(json["label"], "ui");
    assert_eq!(json["depth"], 0);
    assert!(json["run_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(
        turns.lock().unwrap().clone(),
        vec![vec!["act".to_string()], vec!["act".to_string()]]
    );
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_results_reach_after_tool_as_plain_data_too() {
    // `ToolOutput` is yoagent's type; a Rust handler edits it in place.
    let (root, bridge) = setup();
    let p = root.plugin(plugin(handler("marker").with_after_tool(|_, output| {
        output.result = ToolResult {
            content: vec![Content::Text {
                text: "replaced".into(),
            }],
            details: serde_json::json!({"marked": true}),
        };
        output.is_error = true;
        Ok(())
    })));
    wait_active(&p).await;
    let (agent, _) = agent(vec![call("act", serde_json::json!({})), text("done")]);
    let mut agent = agent
        .with_tools(vec![Box::new(Reply::new("act", "acted"))])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert_eq!(results, vec![("act".into(), "replaced".into(), true)]);
    root.shutdown().await.unwrap();
}
