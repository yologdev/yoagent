//! Tests for `HttpTransport` — the request/response subset of MCP Streamable
//! HTTP (issue #82), plus the plain JSON-RPC behavior it must not regress.
//!
//! Response bodies interpolate the *real* request id. Ids come from a
//! process-global counter (`types::next_request_id`), so a hardcoded `"id":1`
//! would never match, and a suite full of them would quietly encode "we don't
//! correlate responses to requests" — which is the bug these tests exist to
//! prevent. Each test therefore builds its request first and mounts the mock
//! with that id.

use wiremock::matchers::{body_string_contains, header, method};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use yoagent::mcp::transport::McpTransport;
use yoagent::mcp::types::JsonRpcRequest;
use yoagent::mcp::{HttpTransport, McpClient};

/// Matcher: the request must NOT carry the given header.
struct HeaderAbsent(&'static str);

impl wiremock::Match for HeaderAbsent {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.0)
    }
}

/// Matcher: the named header's value contains the given substring.
///
/// `wiremock::matchers::header` splits the *request's* value on commas and
/// compares the whole resulting list against the expected value wrapped in a
/// one-element list, so it can never match a multi-value header like
/// `Accept: application/json, text/event-stream`.
struct HeaderContains(&'static str, &'static str);

impl wiremock::Match for HeaderContains {
    fn matches(&self, request: &Request) -> bool {
        request
            .headers
            .get(self.0)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains(self.1))
    }
}

fn sse(payload: &str) -> String {
    format!("event: message\ndata: {payload}\n\n")
}

async fn mount_body(server: &MockServer, body: String, content_type: &str) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, content_type))
        .mount(server)
        .await;
}

/// Backward compatibility: a plain JSON-RPC body keeps working. This was the
/// only shape supported before #82.
#[tokio::test]
async fn plain_json_response_still_parses() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("ping", None);
    mount_body(
        &server,
        format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"ok":true}}}}"#,
            request.id
        ),
        "application/json",
    )
    .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport
        .send(request)
        .await
        .expect("plain JSON must parse");

    assert_eq!(response.result.unwrap()["ok"], true);
}

/// The reported failure: servers returning `text/event-stream` used to hit
/// `Response parse error` because the body was parsed as a single JSON object.
#[tokio::test]
async fn sse_framed_response_parses() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/list", None);
    mount_body(
        &server,
        sse(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"tools":[]}}}}"#,
            request.id
        )),
        "text/event-stream",
    )
    .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport.send(request).await.expect("SSE must parse");

    assert!(response.result.unwrap()["tools"].is_array());
}

/// A UTF-8 BOM is legal at the head of a JSON document and .NET/JVM servers
/// emit one. `Response::text()` sniffed it away; reading the body as bytes does
/// not, and `trim()` will not remove it either — U+FEFF stopped being
/// White_Space in Unicode 4.0 — so it breaks the parse and the error names a
/// body that visibly contains the response.
#[tokio::test]
async fn bom_prefixed_plain_json_body_still_parses() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("ping", None);
    mount_body(
        &server,
        format!(
            "\u{feff}{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"ok\":true}}}}",
            request.id
        ),
        "application/json",
    )
    .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport
        .send(request)
        .await
        .expect("a BOM must not hide the response");
    assert_eq!(response.result.unwrap()["ok"], true);
}

/// A declared non-UTF-8 charset must fail loudly. Reading the body as bytes
/// gives up the transcoding `Response::text()` did, and decoding such a body as
/// UTF-8 anyway would return silently mangled tool output as a success.
#[tokio::test]
async fn non_utf8_charset_is_refused_rather_than_mangled() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("ping", None);
    mount_body(
        &server,
        format!(r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, request.id),
        "application/json; charset=iso-8859-1",
    )
    .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let err = transport
        .send(request)
        .await
        .expect_err("a non-UTF-8 charset must not be silently decoded as UTF-8");
    assert!(err.to_string().contains("charset"), "got: {err}");
}

/// The one shape only the `method` check rejects: a malformed frame carrying
/// both a `method` and a `result`. Without this, that check is unkillable by
/// the suite and a maintainer doing dead-code cleanup would delete it green.
#[tokio::test]
async fn frame_with_both_method_and_result_is_not_the_response() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/list", None);
    let body = format!(
        concat!(
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"notifications/progress\",\"result\":{{\"bogus\":true}}}}\n\n",
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"real\":true}}}}\n\n"
        ),
        id = request.id
    );
    mount_body(&server, body, "text/event-stream").await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport.send(request).await.expect("send");

    assert_eq!(
        response.result.expect("the well-formed response must win")["real"],
        true
    );
}

/// Frames that aren't JSON-RPC at all — comments, `event:`/`id:` lines, foreign
/// payloads — must be skipped.
#[tokio::test]
async fn sse_skips_frames_that_are_not_json_rpc() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/list", None);
    let body = format!(
        concat!(
            ": keep-alive comment\n\n",
            "event: ping\ndata: {{\"unrelated\":true}}\n\n",
            "id: 42\nevent: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"found\":true}}}}\n\n"
        ),
        request.id
    );
    mount_body(&server, body, "text/event-stream").await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport
        .send(request)
        .await
        .expect("payload must be found");

    assert_eq!(response.result.unwrap()["found"], true);
}

/// The bug this suite previously missed. A Streamable HTTP server may emit
/// `notifications/progress` and `notifications/message` on the POST response
/// stream *before* the result — that is how it reports progress and logs during
/// a `tools/call`.
///
/// Those frames deserialize into `JsonRpcResponse` (every field but `jsonrpc`
/// is optional, and unknown keys are ignored), so a parser that accepts the
/// first thing that *parses* returns the notification and silently discards the
/// answer. Selection must be structural: no `method`, and a `result` or `error`.
#[tokio::test]
async fn notification_frames_do_not_shadow_the_response() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/list", None);
    let body = format!(
        concat!(
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{{\"level\":\"info\",\"data\":\"searching\"}}}}\n\n",
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"progress\":1}}}}\n\n",
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"tools\":[{{\"name\":\"web_search\"}}]}}}}\n\n"
        ),
        request.id
    );
    mount_body(&server, body, "text/event-stream").await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport.send(request).await.expect("send");

    let result = response
        .result
        .expect("the result must win over preceding notification frames");
    assert_eq!(result["tools"][0]["name"], "web_search");
}

/// A server→client *request* (e.g. `sampling/createMessage`) also carries a
/// `method` and an id that is not ours. It must not be mistaken for the answer.
#[tokio::test]
async fn server_initiated_request_does_not_shadow_the_response() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/call", None);
    let body = format!(
        concat!(
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"sampling/createMessage\",\"params\":{{}}}}\n\n",
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"content\":[]}}}}\n\n"
        ),
        request.id
    );
    mount_body(&server, body, "text/event-stream").await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport.send(request).await.expect("send");

    assert!(
        response.result.is_some(),
        "the real result must be selected"
    );
}

/// A bare ack frame carries our id but neither a result nor an error. It has no
/// `method`, so only the result-or-error check rejects it — without that, it
/// would be returned as the answer and the real result discarded.
#[tokio::test]
async fn bare_ack_frame_does_not_shadow_the_response() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/list", None);
    let body = format!(
        concat!(
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id}}}\n\n",
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"real\":true}}}}\n\n"
        ),
        id = request.id
    );
    mount_body(&server, body, "text/event-stream").await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport.send(request).await.expect("send");

    assert_eq!(
        response
            .result
            .expect("the real result must win over a bare ack")["real"],
        true
    );
}

/// Guards the *natural wrong fix* for the shadowing bug. Filtering on
/// `result.is_some()` alone would convert every server-side JSON-RPC error into
/// an opaque transport failure, hiding the reason a tool call was rejected.
#[tokio::test]
async fn json_rpc_error_over_sse_reaches_the_caller() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/call", None);
    let body = format!(
        concat!(
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{{}}}}\n\n",
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":-32602,\"message\":\"path must be absolute\"}}}}\n\n"
        ),
        request.id
    );
    mount_body(&server, body, "text/event-stream").await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport.send(request).await.expect("send");

    let error = response.error.expect("the server's error must survive");
    assert_eq!(error.code, -32602);
    assert!(error.message.contains("absolute"));
}

/// A response for a different request must not be accepted as ours.
#[tokio::test]
async fn mismatched_response_id_is_rejected() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/list", None);
    mount_body(
        &server,
        sse(r#"{"jsonrpc":"2.0","id":999999,"result":{"stale":true}}"#),
        "text/event-stream",
    )
    .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let err = transport
        .send(request)
        .await
        .expect_err("a response for another id must not be returned as ours");
    assert!(
        err.to_string().contains("no JSON-RPC response"),
        "got: {err}"
    );
}

/// SSE joins an event's multiple `data:` lines with newlines. A server that
/// pretty-prints its JSON across lines is spec-legal.
#[tokio::test]
async fn multi_line_data_frames_are_joined() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("tools/list", None);
    let body = format!(
        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\ndata: \"id\":{},\ndata: \"result\":{{\"joined\":true}}}}\n\n",
        request.id
    );
    mount_body(&server, body, "text/event-stream").await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let response = transport.send(request).await.expect("send");

    assert_eq!(response.result.unwrap()["joined"], true);
}

/// Streamable HTTP assigns a session on `initialize` and expects it echoed on
/// every subsequent request.
#[tokio::test]
async fn session_id_is_captured_then_replayed() {
    let server = MockServer::start().await;
    let first = JsonRpcRequest::new("initialize", None);
    let second = JsonRpcRequest::new("tools/list", None);

    Mock::given(method("POST"))
        .and(HeaderAbsent("mcp-session-id"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "sess-abc123")
                .set_body_raw(
                    sse(&format!(
                        r#"{{"jsonrpc":"2.0","id":{},"result":{{"initialized":true}}}}"#,
                        first.id
                    )),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(header("mcp-session-id", "sess-abc123"))
        .and(body_string_contains("tools/list"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"tools":[]}}}}"#,
                second.id
            )),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    transport.send(first).await.expect("initialize");
    transport
        .send(second)
        .await
        .expect("follow-up must reuse the session");
}

/// A 404 on a session-bearing request means the server dropped the session.
/// Keeping the dead id would make every later call fail identically with an
/// error that reads like a bad URL.
#[tokio::test]
async fn expired_session_is_cleared_and_named() {
    let server = MockServer::start().await;
    let first = JsonRpcRequest::new("initialize", None);

    Mock::given(method("POST"))
        .and(HeaderAbsent("mcp-session-id"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "sess-dead")
                .set_body_raw(
                    format!(r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, first.id),
                    "application/json",
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("mcp-session-id", "sess-dead"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    transport.send(first).await.expect("initialize");

    let err = transport
        .send(JsonRpcRequest::new("tools/list", None))
        .await
        .expect_err("404 must surface");
    assert!(
        err.to_string().contains("session expired"),
        "the error must name the cause, got: {err}"
    );

    // The dead session is gone: the next request goes out without one, so a
    // caller that rebuilds the handshake can recover. (Asserted on the wire
    // rather than on the response, because a fresh request carries a fresh id
    // that this test's canned body cannot know.)
    let _ = transport
        .send(JsonRpcRequest::new("initialize", None))
        .await;

    let last = server
        .received_requests()
        .await
        .unwrap()
        .pop()
        .expect("a third request was sent");
    assert!(
        !last.headers.contains_key("mcp-session-id"),
        "the expired session must not be replayed"
    );
}

/// Notifications get `202 Accepted` with no body. `send()` must still return a
/// response rather than failing a call that succeeded.
#[tokio::test]
async fn accepted_with_empty_body_is_not_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let request = JsonRpcRequest::new("notifications/initialized", None);
    let request_id = request.id;

    let response = transport
        .send(request)
        .await
        .expect("202 with no body must not be an error");

    assert_eq!(response.id, Some(request_id));
    assert!(response.result.is_none() && response.error.is_none());
}

/// But an empty body on a *non-ack* status is a real failure — a proxy
/// answering instead of the MCP server, or a drained upstream. Synthesizing a
/// success there would hide it.
#[tokio::test]
async fn empty_body_on_plain_200_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let err = transport
        .send(JsonRpcRequest::new("tools/list", None))
        .await
        .expect_err("an empty 200 must not be dressed up as success");
    assert!(err.to_string().contains("empty body"), "got: {err}");
}

/// Every request advertises both framings so a Streamable HTTP server can pick.
#[tokio::test]
async fn accept_header_advertises_both_framings() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("ping", None);
    Mock::given(method("POST"))
        .and(HeaderContains("accept", "application/json"))
        .and(HeaderContains("accept", "text/event-stream"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, request.id),
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    transport.send(request).await.expect("send");
}

/// A non-2xx body carries the server's explanation. Dropping it leaves the user
/// a bare status code to guess from.
#[tokio::test]
async fn non_2xx_preserves_the_server_explanation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_raw(
            r#"{"error":{"message":"Mcp-Session-Id header required"}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let err = transport
        .send(JsonRpcRequest::new("tools/list", None))
        .await
        .expect_err("400 must surface");
    assert!(
        err.to_string().contains("Mcp-Session-Id header required"),
        "the server's explanation must survive, got: {err}"
    );
}

/// `close()` releases the session server-side.
#[tokio::test]
async fn close_deletes_the_session_when_one_exists() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("initialize", None);
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "sess-xyz")
                .set_body_raw(
                    format!(r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, request.id),
                    "application/json",
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(header("mcp-session-id", "sess-xyz"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    transport.send(request).await.expect("initialize");
    transport.close().await.expect("close");
}

/// Without a session there is nothing to release — `close()` must stay the
/// pre-#82 no-op and send nothing at all.
#[tokio::test]
async fn close_without_a_session_sends_nothing() {
    let server = MockServer::start().await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    transport.close().await.expect("close must be a no-op");

    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "close() without a session must not touch the network"
    );
}

/// A server that rejects DELETE must not turn a successful run into an error.
#[tokio::test]
async fn close_ignores_a_server_that_rejects_delete() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("initialize", None);
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "sess-xyz")
                .set_body_raw(
                    format!(r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, request.id),
                    "application/json",
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    transport.send(request).await.expect("initialize");
    transport
        .close()
        .await
        .expect("a 405 on DELETE must not fail close()");
}

/// An unparseable body must still be a loud error, with a snippet to debug from.
#[tokio::test]
async fn unparseable_body_is_an_error_with_context() {
    let server = MockServer::start().await;
    mount_body(&server, "<html>gateway</html>".into(), "text/html").await;

    let transport = HttpTransport::new(&server.uri()).unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .expect_err("an unparseable body must be an error");

    assert!(
        err.to_string().contains("gateway"),
        "error should quote the body, got: {err}"
    );
}

/// End to end through the public entry point: `McpClient::connect_http` runs the
/// real handshake (initialize → notifications/initialized → tools/list) against
/// a Streamable HTTP server that SSE-frames its responses, assigns a session,
/// and emits a log notification ahead of the result.
///
/// This is the path issue #82 is actually about, and it had no coverage — the
/// nine transport tests all construct `HttpTransport` by hand, and the
/// `tool_adapter` tests use `from_transport`, which skips `initialize` entirely.
#[tokio::test]
async fn e2e_handshake_over_streamable_http() {
    let server = MockServer::start().await;

    // `body_string_contains("initialize")` would also match
    // `notifications/initialized`, so match the method field exactly.
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "sess-e2e")
                .set_body_raw(
                    sse(r#"{"jsonrpc":"2.0","result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"probe","version":"1.0"}}}"#),
                    "text/event-stream",
                ),
        )
        .mount(&server)
        .await;

    // `initialized` is a notification: sent once, with the session, and
    // without an id (it used to be a request awaiting a reply).
    Mock::given(method("POST"))
        .and(body_string_contains("notifications/initialized"))
        .and(header("mcp-session-id", "sess-e2e"))
        .and(|req: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&req.body)
                .map(|v| v.get("id").is_none())
                .unwrap_or(false)
        })
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(body_string_contains("tools/list"))
        .and(header("mcp-session-id", "sess-e2e"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\"}}\n\n",
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"tools\":[{\"name\":\"web_search\",\"inputSchema\":{}}]}}\n\n"
            ),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let client = McpClient::connect_http(&server.uri())
        .await
        .expect("handshake over SSE must succeed");
    assert_eq!(client.server_info().unwrap().name, "probe");

    let tools = client.list_tools().await.expect("tools/list over SSE");
    assert_eq!(tools[0].name, "web_search");
}

/// A server that rejects `notifications/initialized` has not finished the
/// handshake: the connect fails, quoting the server, instead of failing later
/// at `tools/list`.
#[tokio::test]
async fn a_rejected_initialized_notification_fails_the_connect() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"jsonrpc":"2.0","result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"strict","version":"1"}}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("notifications/initialized"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad notification"))
        .mount(&server)
        .await;

    let err = McpClient::connect_http(&server.uri())
        .await
        .err()
        .expect("the handshake did not complete")
        .to_string();
    assert!(
        err.contains("400") && err.contains("bad notification"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Caller headers (#275)
// ---------------------------------------------------------------------------

/// Answers every JSON-RPC request (`initialize`, `tools/list`, `tools/call`)
/// with a session id and a minimal result, `202` for notifications and `204`
/// for the closing `DELETE` — but only when both caller headers are present.
/// A request without them matches nothing and gets wiremock's 404.
async fn mount_server_requiring(server: &MockServer, auth: &'static str, agent: &'static str) {
    Mock::given(method("POST"))
        .and(header("authorization", auth))
        .and(header("user-agent", agent))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body.get("id").is_none() {
                return ResponseTemplate::new(202);
            }
            let result = match body["method"].as_str().unwrap() {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "auth-fixture", "version": "1"}
                }),
                "tools/list" => serde_json::json!({"tools": [
                    {"name": "whoami", "inputSchema": {"type": "object"}}
                ]}),
                _ => serde_json::json!({"content": [{"type": "text", "text": "you"}]}),
            };
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "auth-session")
                .set_body_json(
                    serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": result}),
                )
        })
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .and(header("authorization", auth))
        .and(header("user-agent", agent))
        .and(header("mcp-session-id", "auth-session"))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

/// The headers reach every request of a session: the handshake (initialize and
/// the initialized notification), discovery, a call and the closing DELETE.
#[tokio::test]
async fn caller_headers_go_on_every_request_of_the_session() {
    let server = MockServer::start().await;
    mount_server_requiring(&server, "Bearer s3cret", "my-app/1.0").await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_header("Authorization", "Bearer s3cret")
        .unwrap()
        .with_header("user-agent", "my-app/1.0")
        .unwrap();
    let client = McpClient::connect_http_with(transport).await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools[0].name, "whoami");
    client
        .call_tool("whoami", serde_json::json!({}))
        .await
        .unwrap();
    client.close().await.unwrap();

    let requests = server.received_requests().await.unwrap();
    // initialize, notifications/initialized, tools/list, tools/call, DELETE —
    // each matched the header-requiring mocks (a miss would have been a 404).
    let methods: Vec<&str> = requests.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(methods, ["POST", "POST", "POST", "POST", "DELETE"]);
    // `close` treats a rejected DELETE as best-effort, so check it directly.
    for r in &requests {
        assert_eq!(r.headers.get("authorization").unwrap(), "Bearer s3cret");
        assert_eq!(r.headers.get("user-agent").unwrap(), "my-app/1.0");
    }
}

/// An agent connects the same way, and discovers the server's tools.
#[tokio::test]
async fn an_agent_connects_through_a_configured_transport() {
    let server = MockServer::start().await;
    mount_server_requiring(&server, "Bearer s3cret", "my-app/1.0").await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_header("authorization", "Bearer s3cret")
        .unwrap()
        .with_header("user-agent", "my-app/1.0")
        .unwrap();
    use yoagent::provider::mock::{MockResponse, MockToolCall};
    let provider = yoagent::provider::MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "whoami".into(),
            arguments: serde_json::json!({}),
            provider_metadata: None,
        }]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = yoagent::Agent::from_provider(provider, yoagent::provider::ModelConfig::mock())
        .with_mcp_server_http_transport(transport)
        .await
        .unwrap();
    let mut events = agent.prompt("who am I").await;
    let mut called = false;
    while let Some(event) = events.recv().await {
        if let yoagent::AgentEvent::ToolExecutionEnd {
            tool_name,
            is_error,
            ..
        } = event
        {
            called = tool_name == "whoami" && !is_error;
        }
    }
    agent.finish().await;
    assert!(called, "the discovered tool ran with the caller headers");
}

/// Without the header the same server refuses the handshake — the header is
/// what the tests above depend on, not an accident of the fixture.
#[tokio::test]
async fn without_the_header_the_server_refuses_the_connect() {
    let server = MockServer::start().await;
    mount_server_requiring(&server, "Bearer s3cret", "my-app/1.0").await;
    let err = McpClient::connect_http(&server.uri()).await.err().unwrap();
    assert!(err.to_string().contains("404"), "{err}");
}

#[test]
fn invalid_or_reserved_headers_are_refused_before_sending() {
    let new = || HttpTransport::new("http://127.0.0.1:9/mcp").unwrap();
    assert!(new().with_header("bad header", "x").is_err());
    let err = new()
        .with_header("authorization", "Bearer s3cret\r\nInjected: 1")
        .err()
        .unwrap()
        .to_string();
    assert!(
        !err.contains("s3cret"),
        "a refused value is never echoed: {err}"
    );
    // Not visible ASCII: wasm32's client would refuse it on every request.
    assert!(new().with_header("x-user", "café").is_err());
    for reserved in [
        "Accept",
        "content-type",
        "MCP-Session-Id",
        "Content-Length",
        "host",
    ] {
        let err = new().with_header(reserved, "x").err().unwrap().to_string();
        assert!(
            err.contains("set by the MCP transport"),
            "{reserved}: {err}"
        );
    }
}

/// Setting a header twice keeps only the second value.
#[tokio::test]
async fn setting_a_header_twice_replaces_it() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("ping", None);
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer new"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": request.id, "result": {}}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    HttpTransport::new(&server.uri())
        .unwrap()
        .with_header("authorization", "Bearer old")
        .unwrap()
        .with_header("Authorization", "Bearer new")
        .unwrap()
        .send(request)
        .await
        .unwrap();
    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received[0].headers.get_all("authorization").iter().count(),
        1
    );
    server.verify().await;
}

/// No headers configured: nothing extra is sent (no User-Agent either).
#[tokio::test]
async fn a_plain_transport_sends_no_caller_headers() {
    let server = MockServer::start().await;
    let request = JsonRpcRequest::new("ping", None);
    Mock::given(method("POST"))
        .and(HeaderAbsent("authorization"))
        .and(HeaderAbsent("user-agent"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": request.id, "result": {}}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    HttpTransport::new(&server.uri())
        .unwrap()
        .send(request)
        .await
        .unwrap();
    server.verify().await;
}
