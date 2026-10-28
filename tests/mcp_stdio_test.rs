//! `StdioTransport` against scripted servers that behave like real ones:
//! interleaved notifications and server requests, a stale response, a noisy
//! (and not always UTF-8) stderr, a content kind this crate does not model,
//! late answers, servers that die. Unix only (the servers are bash scripts).
#![cfg(all(feature = "native", unix))]

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use yoagent::mcp::{McpClient, McpToolAdapter};
use yoagent::types::*;

/// Shared by the scripts: the numeric id of a JSON-RPC line.
const ID_OF: &str = r#"id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }"#;

const SERVER: &str = r#"
# initialize: a banner line, a notification, a ping and a sampling request
# arrive before the answer.
read -r line; id=$(id_of "$line")
echo 'fake-server v1 starting (a banner on stdout)'
echo '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"starting"}}'
echo '{"jsonrpc":"2.0","id":"srv-1","method":"ping"}'
read -r pong
case "$pong" in *'"id":"srv-1"'*'"result"'*) ;; *) echo "bad ping reply: $pong" >&2; exit 1;; esac
echo '{"jsonrpc":"2.0","id":"srv-2","method":"sampling/createMessage","params":{}}'
read -r declined
case "$declined" in *'"error"'*'-32601'*'"id":"srv-2"'*) ;; *) echo "sampling not declined: $declined" >&2; exit 1;; esac
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fake\",\"version\":\"1\"}}}"

# notifications/initialized: no id, and no reply is sent.
read -r note
case "$note" in *'"id"'*) echo "the notification carried an id: $note" >&2; exit 1;; esac

# tools/list: a stale response for another id comes first.
read -r line; id=$(id_of "$line")
echo '{"jsonrpc":"2.0","id":999999,"result":{"tools":[]}}'
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"fetch\",\"description\":\"Fetch\",\"inputSchema\":{\"type\":\"object\"}}]}}"

# Call 1: stderr gets a non-UTF-8 byte, then 200 KB (more than a pipe
# holds), then the answer: a text block and a resource block.
read -r line; id=$(id_of "$line")
printf '\377 not utf-8\n' >&2
head -c 200000 /dev/zero | tr '\0' x >&2
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"fetched\"},{\"type\":\"resource\",\"resource\":{\"uri\":\"file:///a.txt\",\"text\":\"body\"}}]}}"

# Call 2: not answered in time (the client times out)...
read -r line; late=$(id_of "$line")
# Call 3: ...its answer arrives late, ahead of call 3's own.
read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$late,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"too late\"}]}}"
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"third\"}]}}"

# Every later call: never answered.
while read -r line; do :; done
"#;

fn ctx() -> ToolContext {
    ToolContext::new("call-1", "fetch")
}

fn text_of(result: &ToolResult, i: usize) -> &str {
    match &result.content[i] {
        Content::Text { text } => text,
        other => panic!("expected text, got {other:?}"),
    }
}

async fn connect(script: &str) -> Result<McpClient, yoagent::mcp::McpError> {
    let script = format!("{ID_OF}\n{script}");
    McpClient::connect_stdio("bash", &["-c", &script], None).await
}

#[tokio::test]
async fn a_realistic_stdio_server_works_end_to_end() {
    let run = async {
        let client = connect(SERVER)
            .await
            .expect("the handshake completes despite the banner and server requests");
        assert!(client.call_timeout().is_some(), "stdio calls are bounded");
        let client = Arc::new(Mutex::new(client));
        let tools = McpToolAdapter::from_client(client.clone())
            .await
            .expect("tools/list past the stale response");
        assert_eq!(tools.len(), 1);
        let tool = tools.into_iter().next().unwrap();

        // Call 1.
        let result = tool
            .execute(serde_json::json!({"url": "x"}), ctx())
            .await
            .expect("the call completes despite a non-UTF-8, flooded stderr");
        assert_eq!(text_of(&result, 0), "fetched");
        assert_eq!(text_of(&result, 1), "[resource file:///a.txt]\nbody");

        // Call 2 times out...
        let short = tool.with_call_timeout(Some(Duration::from_millis(300)));
        let err = short
            .execute(serde_json::json!({}), ctx())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no answer within 300ms") && err.contains("may still carry it out"),
            "{err}"
        );

        // ...and call 3 gets its own answer, not call 2's late one.
        let tool = short.with_call_timeout(Some(Duration::from_secs(10)));
        let result = tool.execute(serde_json::json!({}), ctx()).await.unwrap();
        assert_eq!(text_of(&result, 0), "third");

        // A call the run cancels ends at once.
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let err = tool
            .with_call_timeout(None)
            .execute(serde_json::json!({}), ctx().with_cancel(cancel))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled), "{err:?}");
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("nothing hangs");
}

/// A server that dies at startup explains itself on stderr; the error says
/// so, with its exit status, instead of a bare "Connection closed".
#[tokio::test]
async fn a_server_that_dies_reports_its_stderr() {
    let err = connect(
        r#"read -r line
echo "error: OPENAI_API_KEY is not set" >&2
exit 3"#,
    )
    .await
    .err()
    .expect("the connect fails")
    .to_string();
    assert!(err.contains("Connection closed"), "{err}");
    assert!(err.contains("exit status: 3"), "{err}");
    assert!(err.contains("OPENAI_API_KEY is not set"), "{err}");
}

/// Dropping the client kills the server, even one that ignores EOF on its
/// stdin.
#[tokio::test]
async fn dropping_the_client_kills_the_server() {
    let tmp = tempfile::TempDir::new().unwrap();
    let pid_file = tmp.path().join("pid");
    let script = format!(
        r#"echo $$ > '{}'
read -r line; id=$(id_of "$line")
echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"stubborn\",\"version\":\"1\"}}}}}}"
read -r note
trap '' HUP TERM
while :; do sleep 1; done"#,
        pid_file.display()
    );
    let client = connect(&script).await.expect("connects");
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", &pid])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    assert!(alive(), "the server runs while the client lives");
    drop(client);
    for _ in 0..50 {
        if !alive() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the server outlived its client");
}

/// Calls after the server has gone keep failing, cleanly and at once: only
/// the first close error waits (briefly) for the stderr drain.
#[tokio::test]
async fn calls_after_the_server_exits_keep_failing_cleanly() {
    let client = connect(
        r#"read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"brief\",\"version\":\"1\"}}}"
read -r note
echo "bye" >&2
exit 0"#,
    )
    .await
    .expect("connects");
    let _ = client.list_tools().await.unwrap_err();
    let started = std::time::Instant::now();
    for _ in 0..3 {
        let err = client.list_tools().await.unwrap_err().to_string();
        assert!(
            err.contains("Connection closed") || err.contains("Write error"),
            "{err}"
        );
    }
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "later errors waited on the drain again: {:?}",
        started.elapsed()
    );
}

/// A server that closes its output but keeps reading: every later call reaches
/// the close error (not a write error). The first one waits for the stderr
/// drain and consumes it; later ones must not poll that finished task again
/// (which panicked).
#[tokio::test]
async fn repeated_close_errors_do_not_panic() {
    let client = connect(
        r#"read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mute\",\"version\":\"1\"}}}"
read -r note
exec 1>&- 2>&-
while read -r line; do :; done"#,
    )
    .await
    .expect("connects");
    for _ in 0..3 {
        let err = client.list_tools().await.unwrap_err().to_string();
        assert!(err.contains("Connection closed"), "{err}");
    }
}

/// A server whose tool list grows when its `load` tool is called: it sends
/// `notifications/tools/list_changed` ahead of the call's answer, and lists
/// `extra` from then on.
const LOADING_SERVER: &str = r#"
read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"loader\",\"version\":\"1\"}}}"
read -r note
loaded=0
tool() { printf '{"name":"%s","description":"%s","inputSchema":{"type":"object"}}' "$1" "$1"; }
while read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"tools/list"'*)
      if [ "$loaded" = 1 ]; then tools="[$(tool load),$(tool extra)]"; else tools="[$(tool load)]"; fi
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":$tools}}";;
    *'"name":"load"'*)
      loaded=1
      echo '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}'
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"loaded\"}]}}";;
    *'"name":"extra"'*)
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"extra ran\"}]}}";;
  esac
done
"#;

/// `McpToolSource`: its generation counts the server's
/// `notifications/tools/list_changed`, read while a call is answered, and a
/// listing after it has the new tools.
#[tokio::test]
async fn a_tool_source_follows_the_servers_list_changed() {
    use yoagent::mcp::McpToolSource;
    use yoagent::ToolSource;
    let source = McpToolSource::new(connect(LOADING_SERVER).await.unwrap());
    assert_eq!(source.generation(), 0);
    let tools = source.tools().await;
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names, ["load"]);
    let result = tools[0]
        .execute(serde_json::json!({}), ToolContext::new("c1", "load"))
        .await
        .unwrap();
    assert_eq!(text_of(&result, 0), "loaded");
    assert_eq!(
        source.generation(),
        1,
        "the notification ahead of the answer was counted"
    );
    assert_eq!(source.client().lock().await.tools_list_changes(), 1);
    let names: Vec<String> = source
        .tools()
        .await
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert_eq!(names, ["load", "extra"]);
}

/// Through an agent: the model calls `load`, and in the same run its next
/// request offers — and can call — the tool the server added.
#[tokio::test]
async fn an_agent_offers_a_tool_the_server_adds_mid_run() {
    use yoagent::mcp::McpToolSource;
    use yoagent::provider::mock::{MockResponse, MockToolCall};
    use yoagent::provider::{MockProvider, ModelConfig};
    let call = |name: &str| {
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: name.into(),
            arguments: serde_json::json!({}),
        }])
    };
    let provider = MockProvider::new(vec![
        call("srv__load"),
        call("srv__extra"),
        MockResponse::Text("done".into()),
    ]);
    let source = McpToolSource::new(connect(LOADING_SERVER).await.unwrap()).with_prefix("srv");
    let mut agent =
        yoagent::Agent::from_provider(provider, ModelConfig::mock()).with_tool_source(source);
    let mut rx = agent.prompt("go").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let results: Vec<(String, bool)> = agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult {
                content, is_error, ..
            }) => Some((
                content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
                *is_error,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![
            ("loaded".to_string(), false),
            ("extra ran".to_string(), false)
        ]
    );
}

/// Like `LOADING_SERVER`, but every `tools/list` after the first fails.
const FAILING_LIST_SERVER: &str = r#"
read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"flaky\",\"version\":\"1\"}}}"
read -r note
listed=0
while read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"tools/list"'*)
      if [ "$listed" = 0 ]; then
        listed=1
        echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"load\",\"description\":\"load\",\"inputSchema\":{\"type\":\"object\"}}]}}"
      else
        echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32603,\"message\":\"listing broke\"}}"
      fi;;
    *'"name":"load"'*)
      echo '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}'
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"loaded\"}]}}";;
  esac
done
"#;

/// A `tools/list` that fails after a change keeps the last list that
/// succeeded: the server's tools stay offered.
#[tokio::test]
async fn a_failed_relisting_keeps_the_last_good_list() {
    use yoagent::mcp::McpToolSource;
    use yoagent::ToolSource;
    let source = McpToolSource::new(connect(FAILING_LIST_SERVER).await.unwrap());
    let tools = source.tools().await;
    assert_eq!(tools.len(), 1);
    tools[0]
        .execute(serde_json::json!({}), ToolContext::new("c1", "load"))
        .await
        .unwrap();
    assert_eq!(source.generation(), 1);
    let names: Vec<String> = source
        .tools()
        .await
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert_eq!(names, ["load"], "the failed listing kept the last good one");
}

/// Answers `initialize`, then each line it is sent with nothing; `quit`
/// (a `tools/call` of it) makes it write to stderr and exit 3.
const QUITTING_SERVER: &str = r#"
read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"quitter\",\"version\":\"1\"}}}"
read -r note
while read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"tools/list"'*)
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"quit\",\"description\":\"quit\",\"inputSchema\":{\"type\":\"object\"}},{\"name\":\"hang\",\"description\":\"hang\",\"inputSchema\":{\"type\":\"object\"}}]}}";;
    *'"name":"quit"'*)
      echo 'out of memory' >&2
      exit 3;;
  esac
done
"#;

/// Poll `exited` until it answers (the process is reaped asynchronously).
async fn wait_exited(client: &McpClient) -> String {
    for _ in 0..200 {
        if let Some(why) = client.exited() {
            return why;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the server's exit was never seen");
}

/// `exited`: `None` while the server runs; once it has gone, its exit status
/// and the last of its stderr — and it stays so.
#[tokio::test]
async fn exited_says_why_a_server_went() {
    let client = connect(QUITTING_SERVER).await.unwrap();
    assert_eq!(client.exited(), None);
    let err = client
        .call_tool("quit", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Connection closed"), "{err}");
    let why = wait_exited(&client).await;
    assert!(why.contains("exit status: 3"), "{why}");
    assert!(why.contains("out of memory"), "{why}");
    assert_eq!(client.exited(), Some(why), "it stays answered");
}

/// `exited` answers while a call is in flight (it does not use the lock a
/// call holds): a host polls it.
#[tokio::test]
async fn exited_answers_during_a_call() {
    let client = Arc::new(connect(QUITTING_SERVER).await.unwrap());
    let calling = client.clone();
    let call = tokio::spawn(async move {
        let _ = calling.call_tool("hang", serde_json::json!({})).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!call.is_finished(), "the call is in flight");
    assert_eq!(client.exited(), None);
    call.abort();
}

/// A server that exits between calls — no call sees it — is seen by
/// `exited`, with its last stderr.
#[tokio::test]
async fn exited_sees_a_server_that_exits_between_calls() {
    let client = connect(
        r#"read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"brief\",\"version\":\"1\"}}}"
read -r note
sleep 0.2
echo bye >&2
exit 4"#,
    )
    .await
    .unwrap();
    let why = wait_exited(&client).await;
    assert!(
        why.contains("exit status: 4") && why.contains("bye"),
        "{why}"
    );
}

/// A server killed by a signal says so.
#[tokio::test]
async fn exited_names_the_signal_that_killed_a_server() {
    let client = connect(
        r#"read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"killed\",\"version\":\"1\"}}}"
read -r note
kill -9 $$"#,
    )
    .await
    .unwrap();
    let why = wait_exited(&client).await;
    assert!(why.contains("signal: 9"), "{why}");
}

/// An exit watch kept after its client is dropped does not keep the server
/// alive: dropping the client still kills it.
#[tokio::test]
async fn a_kept_exit_watch_does_not_keep_the_server_alive() {
    use yoagent::mcp::{McpTransport, StdioTransport};
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let script = format!(
        "echo $$ > {:?}; while read -r line; do :; done",
        pid_file.display().to_string()
    );
    let transport = StdioTransport::new("bash", &["-c", &script], None)
        .await
        .unwrap();
    let watch = transport.exit_watch().unwrap();
    let client = McpClient::from_transport(Box::new(transport));
    let mut pid = String::new();
    for _ in 0..200 {
        pid = std::fs::read_to_string(&pid_file).unwrap_or_default();
        if !pid.trim().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let pid = pid.trim().to_string();
    assert!(!pid.is_empty(), "the server wrote its pid");
    drop(client);
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    for _ in 0..200 {
        if !alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!alive(), "the server outlived its client");
    let why = watch.exited().expect("the watch says it went");
    assert!(why.contains("its client was dropped"), "{why}");
}

/// A server stopped by `close` reads as closed, not as a crash: a host that
/// restarts crashed servers can tell.
#[tokio::test]
async fn a_closed_server_reads_as_closed() {
    let client = connect(QUITTING_SERVER).await.unwrap();
    client.close().await.unwrap();
    let why = wait_exited(&client).await;
    assert!(why.contains("was closed (`close`)"), "{why}");
}

/// `McpToolSource`: a server that exits is withdrawn — its generation moves,
/// and it gives no tools from then on (not the last good list, whose tools
/// would only fail).
#[tokio::test]
async fn a_tool_source_withdraws_a_server_that_exits() {
    use yoagent::mcp::McpToolSource;
    use yoagent::ToolSource;
    let source = McpToolSource::new(connect(QUITTING_SERVER).await.unwrap());
    let tools = source.tools().await;
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names, ["quit", "hang"]);
    assert_eq!(source.generation(), 0);
    let quit = tools.iter().find(|t| t.name() == "quit").unwrap();
    let err = quit
        .execute(serde_json::json!({}), ToolContext::new("c1", "quit"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Connection closed"), "{err}");
    wait_exited(&*source.client().lock().await).await;
    assert_eq!(source.generation(), 1, "the exit moved the generation");
    assert!(source.tools().await.is_empty(), "the tools were withdrawn");
    assert_eq!(source.generation(), 1, "and it moves once");
}

/// Answers the first `tools/list`; exits on the second (a listing in flight).
const EXITS_WHILE_LISTING: &str = r#"
read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"lister\",\"version\":\"1\"}}}"
read -r note
read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"only\",\"description\":\"only\",\"inputSchema\":{\"type\":\"object\"}}]}}"
read -r line
echo 'listing crashed' >&2
exit 4
"#;

/// A server that exits during a listing: that listing keeps the last list (a
/// failed listing does), and the exit withdraws it from the next ask on.
#[tokio::test]
async fn a_tool_source_withdraws_a_server_that_exits_while_listing() {
    use yoagent::mcp::McpToolSource;
    use yoagent::ToolSource;
    let source = McpToolSource::new(connect(EXITS_WHILE_LISTING).await.unwrap());
    assert_eq!(source.tools().await.len(), 1);
    assert_eq!(
        source.tools().await.len(),
        1,
        "the failed listing kept the last list"
    );
    let why = wait_exited(&*source.client().lock().await).await;
    assert!(
        why.contains("exit status: 4") && why.contains("listing crashed"),
        "{why}"
    );
    assert_eq!(source.generation(), 1);
    assert!(source.tools().await.is_empty());
}

/// Through an agent: the model calls a tool that makes the server exit; the
/// next request offers none of its tools.
#[tokio::test]
async fn an_agent_stops_offering_a_server_that_exits() {
    use yoagent::mcp::McpToolSource;
    use yoagent::provider::mock::{MockResponse, MockToolCall};
    use yoagent::provider::{MockProvider, ModelConfig, StreamConfig, StreamEvent, StreamProvider};
    type Seen = Arc<std::sync::Mutex<Vec<Vec<String>>>>;
    struct Recording(MockProvider, Seen);
    #[async_trait::async_trait]
    impl StreamProvider for Recording {
        async fn stream(
            &self,
            config: StreamConfig,
            tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
            cancel: CancellationToken,
        ) -> Result<Message, yoagent::provider::ProviderError> {
            self.1
                .lock()
                .unwrap()
                .push(config.tools.iter().map(|t| t.name.clone()).collect());
            self.0.stream(config, tx, cancel).await
        }
    }
    let seen = Seen::default();
    let provider = Recording(
        MockProvider::new(vec![
            MockResponse::ToolCalls(vec![MockToolCall {
                name: "quit".into(),
                arguments: serde_json::json!({}),
                provider_metadata: None,
            }]),
            MockResponse::Text("done".into()),
        ]),
        seen.clone(),
    );
    let source = McpToolSource::new(connect(QUITTING_SERVER).await.unwrap());
    let mut agent =
        yoagent::Agent::from_provider(provider, ModelConfig::mock()).with_tool_source(source);
    let mut rx = agent.prompt("go").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0], ["hang", "quit"]);
    assert!(
        seen[1].is_empty(),
        "the dead server's tools were withdrawn: {seen:?}"
    );
}

/// An exit is answered at the first poll that sees it, even while the
/// server's stderr is still held open by a process it left behind (the text
/// may still gain stderr): a tool source must withdraw it at once.
#[tokio::test]
async fn an_exit_is_answered_at_first_sight() {
    let client = connect(
        r#"read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"parent\",\"version\":\"1\"}}}"
read -r note
sleep 3 &
exit 6"#,
    )
    .await
    .unwrap();
    // Long enough for bash to have exited; its child keeps the pipes.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let why = client.exited().expect("seen at the first poll");
    assert!(why.contains("exit status: 6"), "{why}");
}

/// A server that closes its output but whose process lives on (a wrapper
/// outliving the real server) reads as gone once a call has found it so.
#[tokio::test]
async fn a_server_whose_output_closed_reads_as_gone() {
    let client = connect(
        r#"read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"wrapper\",\"version\":\"1\"}}}"
read -r note
exec 1>&-
sleep 3"#,
    )
    .await
    .unwrap();
    assert_eq!(client.exited(), None);
    let err = client.list_tools().await.unwrap_err();
    assert!(err.to_string().contains("closed its output"), "{err}");
    let why = client
        .exited()
        .expect("gone once a call found its output closed");
    assert!(
        why.contains("closed its output (its process is still running)"),
        "{why}"
    );
}

/// `path` as a double-quoted shell word.
fn quoted(path: &std::path::Path) -> String {
    format!("{:?}", path.display().to_string())
}

/// A server like `npx`: a wrapper whose child (the real server) ignores
/// its input closing. Each writes its pid to `<dir>/<name>`.
fn wrapper_server(dir: &std::path::Path) -> String {
    format!(
        r#"echo $$ > {wrapper}; sh -c 'echo $$ > {child}; exec sleep 300' < /dev/null &
read -r line; id=$(id_of "$line")
echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"wrapper\",\"version\":\"1\"}}}}}}"
read -r note
while read -r line; do :; done"#,
        wrapper = quoted(&dir.join("wrapper")),
        child = quoted(&dir.join("child")),
    )
}

async fn pid_in(path: &std::path::Path) -> String {
    for _ in 0..200 {
        let pid = std::fs::read_to_string(path).unwrap_or_default();
        if !pid.trim().is_empty() {
            return pid.trim().to_string();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no pid in {}", path.display());
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Polls until `pid` is gone (killed processes are reaped asynchronously).
async fn until_gone(pid: &str) -> bool {
    for _ in 0..200 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

fn pgid(pid: &str) -> String {
    let out = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", pid])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn connect_with(script: &str, group: bool) -> McpClient {
    use yoagent::mcp::StdioOptions;
    let script = format!("{ID_OF}\n{script}");
    McpClient::connect_stdio_with(
        "bash",
        &["-c", &script],
        None,
        StdioOptions::new().own_process_group(group),
    )
    .await
    .unwrap()
}

/// With a process group of its own: the server is outside the host's group
/// (a Ctrl+C in the host's terminal does not reach it), and dropping the
/// client kills what the server started too.
#[tokio::test]
async fn an_own_process_group_isolates_the_server_and_goes_whole() {
    let dir = tempfile::tempdir().unwrap();
    let client = connect_with(&wrapper_server(dir.path()), true).await;
    let wrapper = pid_in(&dir.path().join("wrapper")).await;
    let child = pid_in(&dir.path().join("child")).await;
    let ours = pgid(&std::process::id().to_string());
    assert_eq!(pgid(&wrapper), wrapper, "it leads its own group");
    assert_ne!(pgid(&wrapper), ours);
    drop(client);
    assert!(until_gone(&child).await, "the server's child went with it");
    assert!(until_gone(&wrapper).await);
}

/// `close` kills the whole group too.
#[tokio::test]
async fn close_kills_an_own_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let client = connect_with(&wrapper_server(dir.path()), true).await;
    let child = pid_in(&dir.path().join("child")).await;
    client.close().await.unwrap();
    assert!(until_gone(&child).await, "the server's child went with it");
}

/// By default (the host's group), dropping the client kills the command
/// alone: a child that ignores its closed input lives on — what
/// `own_process_group` is for.
#[tokio::test]
async fn by_default_only_the_command_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let client = connect_with(&wrapper_server(dir.path()), false).await;
    let wrapper = pid_in(&dir.path().join("wrapper")).await;
    let child = pid_in(&dir.path().join("child")).await;
    drop(client);
    assert!(until_gone(&wrapper).await);
    assert!(alive(&child), "the default leaves it");
    std::process::Command::new("kill")
        .args(["-9", &child])
        .status()
        .unwrap();
}

/// A server that closes its input while its process lives on reads as gone
/// once a call fails to write to it.
#[tokio::test]
async fn a_server_that_stops_reading_reads_as_gone() {
    let client = connect(
        r#"read -r line; id=$(id_of "$line")
echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"deaf\",\"version\":\"1\"}}}"
read -r note
exec 0<&-
sleep 3"#,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(client.exited(), None);
    let err = client.list_tools().await.unwrap_err();
    assert!(err.to_string().contains("error"), "{err}");
    let why = client.exited().expect("gone once a write failed");
    assert!(why.contains("no longer reads its input"), "{why}");
}
