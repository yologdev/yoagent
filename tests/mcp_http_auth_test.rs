//! `HttpTransport::with_authorization`: an `Authorization` header asked for
//! request by request, refreshed on a 401 without losing the MCP session, and
//! the `WWW-Authenticate` challenge on `McpError::Http`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use yoagent::mcp::transport::McpTransport;
use yoagent::mcp::types::JsonRpcRequest;
use yoagent::mcp::{AuthChallenge, AuthorizationSource, HttpTransport, McpClient, McpError};

const CHALLENGE: &str = r#"Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", error="invalid_token""#;

/// A token store: hands out `current`; on a 401 records the challenge and,
/// when it has one, switches to `refreshed`.
struct Tokens {
    current: Mutex<Option<String>>,
    refreshed: Option<String>,
    asked: AtomicUsize,
    challenges: Mutex<Vec<AuthChallenge>>,
}

impl Tokens {
    fn new(current: Option<&str>, refreshed: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(current.map(str::to_owned)),
            refreshed: refreshed.map(str::to_owned),
            asked: AtomicUsize::new(0),
            challenges: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl AuthorizationSource for Tokens {
    async fn authorization(&self) -> Result<Option<String>, McpError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Ok(self.current.lock().unwrap().clone())
    }

    async fn challenged(&self, challenge: &AuthChallenge) -> Result<Option<String>, McpError> {
        self.challenges.lock().unwrap().push(challenge.clone());
        if let Some(fresh) = &self.refreshed {
            *self.current.lock().unwrap() = Some(fresh.clone());
        }
        Ok(self.refreshed.clone())
    }
}

/// An MCP server that accepts only `Authorization: <accepted>` (changeable,
/// to expire a token mid-session); anything else is a 401 with
/// [`CHALLENGE`] and a body echoing the header it got.
async fn mount_server(server: &MockServer, accepted: Arc<Mutex<String>>) {
    let post = accepted.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let got = request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            if got != *post.lock().unwrap() {
                return ResponseTemplate::new(401)
                    .insert_header("WWW-Authenticate", CHALLENGE)
                    .set_body_string(format!("token rejected: {got}"));
            }
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body.get("id").is_none() {
                return ResponseTemplate::new(202);
            }
            let result = match body["method"].as_str().unwrap() {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "oauth-fixture", "version": "1"}
                }),
                "tools/list" => serde_json::json!({"tools": [
                    {"name": "whoami", "inputSchema": {"type": "object"}}
                ]}),
                _ => serde_json::json!({"content": [{"type": "text", "text": "you"}]}),
            };
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "oauth-session")
                .set_body_json(
                    serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": result}),
                )
        })
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(move |request: &Request| {
            let ok = request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                == Some(accepted.lock().unwrap().as_str());
            ResponseTemplate::new(if ok { 204 } else { 401 })
        })
        .mount(server)
        .await;
}

fn header_of(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// The token expires mid-session: the refused call is refreshed and sent once
/// more with the same MCP session, and the source's value goes on every
/// request, the closing DELETE included.
#[tokio::test]
async fn an_expired_token_is_refreshed_and_the_session_kept() {
    let server = MockServer::start().await;
    let accepted = Arc::new(Mutex::new("Bearer first-access-token".to_owned()));
    mount_server(&server, accepted.clone()).await;
    let tokens = Tokens::new(
        Some("Bearer first-access-token"),
        Some("Bearer second-access-token"),
    );
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    let client = McpClient::connect_http_with(transport).await.unwrap();
    assert_eq!(client.list_tools().await.unwrap()[0].name, "whoami");

    // The server expires the first token.
    *accepted.lock().unwrap() = "Bearer second-access-token".into();
    client
        .call_tool("whoami", serde_json::json!({}))
        .await
        .unwrap();
    client.close().await.unwrap();

    let challenges = tokens.challenges.lock().unwrap().clone();
    assert_eq!(challenges.len(), 1, "one refresh");
    assert_eq!(challenges[0].status, 401);
    assert_eq!(challenges[0].method, "tools/call");
    assert_eq!(challenges[0].www_authenticate.as_deref(), Some(CHALLENGE));
    assert_eq!(challenges[0].refused(), Some("Bearer first-access-token"));

    let requests = server.received_requests().await.unwrap();
    // initialize, initialized, tools/list, tools/call (401), tools/call, DELETE.
    assert_eq!(requests.len(), 6);
    let (refused, retried) = (&requests[3], &requests[4]);
    assert_eq!(
        header_of(refused, "authorization").as_deref(),
        Some("Bearer first-access-token")
    );
    assert_eq!(
        header_of(retried, "authorization").as_deref(),
        Some("Bearer second-access-token")
    );
    assert_eq!(refused.body, retried.body, "the same request, sent again");
    for request in &requests[2..] {
        assert_eq!(
            header_of(request, "mcp-session-id").as_deref(),
            Some("oauth-session"),
            "the session survives the refresh"
        );
    }
    assert_eq!(requests[5].method.as_str(), "DELETE");
    assert_eq!(
        header_of(&requests[5], "authorization").as_deref(),
        Some("Bearer second-access-token")
    );
    // Asked once per request sent fresh: 5 requests, the retry took its value
    // from `challenged`.
    assert_eq!(tokens.asked.load(Ordering::SeqCst), 5);
}

/// A source that cannot refresh gives up: the request fails as
/// `McpError::Http` 401 carrying the challenge, sent once, and the token the
/// server echoed is redacted.
#[tokio::test]
async fn a_source_that_gives_up_fails_the_request_with_the_challenge() {
    let server = MockServer::start().await;
    mount_server(&server, Arc::new(Mutex::new("Bearer other".into()))).await;
    let tokens = Tokens::new(Some("Bearer expired-access-token"), None);
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();

    assert_eq!(err.http_status(), Some(401));
    assert_eq!(err.www_authenticate(), Some(CHALLENGE));
    let text = err.to_string();
    assert!(!text.contains("expired-access-token"), "{text}");
    assert!(text.contains("token rejected: [redacted]"), "{text}");
    assert_eq!(tokens.challenges.lock().unwrap().len(), 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// A refreshed token the server refuses too ends the request: one retry, not
/// a loop.
#[tokio::test]
async fn a_refused_retry_is_not_retried_again() {
    let server = MockServer::start().await;
    mount_server(&server, Arc::new(Mutex::new("Bearer neither".into()))).await;
    let tokens = Tokens::new(
        Some("Bearer first-access-token"),
        Some("Bearer also-refused"),
    );
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), Some(401));
    assert_eq!(err.www_authenticate(), Some(CHALLENGE));
    assert!(!err.to_string().contains("also-refused"), "{err}");
    assert!(
        err.to_string()
            .contains("refused again after the authorization source supplied a new value"),
        "{err}"
    );
    assert_eq!(tokens.challenges.lock().unwrap().len(), 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

/// A 403 (`insufficient_scope`) wants a new consent, not a refresh: not
/// retried, the source not asked, the challenge on the error.
#[tokio::test]
async fn a_403_is_not_refreshed_and_carries_its_challenge() {
    let server = MockServer::start().await;
    let scope = r#"Bearer error="insufficient_scope", scope="issues:write""#;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).insert_header("WWW-Authenticate", scope))
        .expect(1)
        .mount(&server)
        .await;
    let tokens = Tokens::new(Some("Bearer an-access-token"), Some("Bearer unused-token"));
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("tools/call", None))
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), Some(403));
    assert_eq!(err.www_authenticate(), Some(scope));
    assert!(tokens.challenges.lock().unwrap().is_empty());
}

/// `Ok(None)` from the source: the request goes without the header.
#[tokio::test]
async fn no_value_sends_no_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            assert!(!request.headers.contains_key("authorization"));
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": {}}),
            )
        })
        .expect(1)
        .mount(&server)
        .await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(Tokens::new(None, None))
        .unwrap();
    transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap();
}

struct Failing;

#[async_trait::async_trait]
impl AuthorizationSource for Failing {
    async fn authorization(&self) -> Result<Option<String>, McpError> {
        Err(McpError::Transport("sign-in required".into()))
    }
}

/// A source that fails fails the request before anything is sent.
#[tokio::test]
async fn a_failing_source_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(Arc::new(Failing))
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("sign-in required"), "{err}");
}

/// A value that is not a valid header is refused without being echoed, and
/// nothing is sent.
#[tokio::test]
async fn an_invalid_value_is_refused_unsent_and_unechoed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(Tokens::new(Some("Bearer secret\nvalue"), None))
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("not a valid header value"), "{text}");
    assert!(!text.contains("secret"), "{text}");
}

/// One transport, one source of credentials: a fixed `Authorization` header
/// and a source together are refused, in either order.
#[test]
fn a_header_and_a_source_cannot_both_set_authorization() {
    let source = Tokens::new(Some("Bearer x"), None);
    let err = HttpTransport::new("https://mcp.example.com/mcp")
        .unwrap()
        .with_header("Authorization", "Bearer fixed")
        .unwrap()
        .with_authorization(source.clone())
        .err()
        .expect("refused");
    assert!(err.to_string().contains("cannot both be set"), "{err}");
    let err = HttpTransport::new("https://mcp.example.com/mcp")
        .unwrap()
        .with_authorization(source)
        .unwrap()
        .with_header("authorization", "Bearer fixed")
        .err()
        .expect("refused");
    assert!(err.to_string().contains("cannot both be set"), "{err}");
}

/// Without a source, a 401's challenge still reaches the caller, and a fixed
/// header's value echoed by the server is redacted.
#[tokio::test]
async fn a_plain_transport_reports_the_challenge_and_redacts_its_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let got = header_of(request, "authorization").unwrap_or_default();
            ResponseTemplate::new(401)
                .insert_header("WWW-Authenticate", CHALLENGE)
                .set_body_string(format!("bad credentials {got}"))
        })
        .mount(&server)
        .await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_header("Authorization", "Bearer fixed-access-token")
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), Some(401));
    assert_eq!(err.www_authenticate(), Some(CHALLENGE));
    let text = err.to_string();
    assert!(!text.contains("fixed-access-token"), "{text}");
    assert!(text.contains("bad credentials [redacted]"), "{text}");
}

/// A JSON-RPC error message that echoes the credential is redacted too.
#[tokio::test]
async fn a_json_rpc_error_echoing_the_token_is_redacted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let got = header_of(request, "authorization").unwrap_or_default();
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0", "id": body["id"],
                "error": {"code": -32000, "message": format!("token {got} lacks a scope")}
            }))
        })
        .mount(&server)
        .await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(Tokens::new(Some("Bearer source-access-token"), None))
        .unwrap();
    let response = transport
        .send(JsonRpcRequest::new("tools/call", None))
        .await
        .unwrap();
    let message = response.error.unwrap().message;
    assert_eq!(message, "token [redacted] lacks a scope");
}

/// Sign-in before any session: the source has nothing yet, `initialize` is
/// refused, the source signs in, and the session the retried `initialize`
/// opens is used from then on.
#[tokio::test]
async fn a_401_on_initialize_signs_in_before_the_session() {
    let server = MockServer::start().await;
    mount_server(
        &server,
        Arc::new(Mutex::new("Bearer signed-in-token".into())),
    )
    .await;
    let tokens = Tokens::new(None, Some("Bearer signed-in-token"));
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    let client = McpClient::connect_http_with(transport).await.unwrap();
    client.list_tools().await.unwrap();

    let challenges = tokens.challenges.lock().unwrap().clone();
    assert_eq!(challenges.len(), 1);
    assert_eq!(challenges[0].method, "initialize");
    assert_eq!(challenges[0].refused(), None, "no value was sent");
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].headers.contains_key("authorization"));
    assert!(!requests[1].headers.contains_key("mcp-session-id"));
    let last = requests.last().unwrap();
    assert_eq!(
        header_of(last, "mcp-session-id").as_deref(),
        Some("oauth-session")
    );
}

/// The `initialized` notification is refreshed like a call, session kept.
#[tokio::test]
async fn a_401_on_a_notification_is_refreshed() {
    let server = MockServer::start().await;
    let accepted = Arc::new(Mutex::new("Bearer first-access-token".to_owned()));
    let flip = accepted.clone();
    // Expire the first token once `initialize` has been answered.
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let got = header_of(request, "authorization").unwrap_or_default();
            if got != *flip.lock().unwrap() {
                return ResponseTemplate::new(401).insert_header("WWW-Authenticate", CHALLENGE);
            }
            if body.get("id").is_none() {
                return ResponseTemplate::new(202);
            }
            *flip.lock().unwrap() = "Bearer second-access-token".into();
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "oauth-session")
                .set_body_json(
                    serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": {
                        "protocolVersion": "2024-11-05", "capabilities": {},
                        "serverInfo": {"name": "oauth-fixture", "version": "1"}
                    }}),
                )
        })
        .mount(&server)
        .await;
    let tokens = Tokens::new(
        Some("Bearer first-access-token"),
        Some("Bearer second-access-token"),
    );
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    McpClient::connect_http_with(transport).await.unwrap();

    let challenges = tokens.challenges.lock().unwrap().clone();
    assert_eq!(challenges.len(), 1);
    assert_eq!(challenges[0].method, "notifications/initialized");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        3,
        "initialize, refused notification, retried notification"
    );
    assert_eq!(requests[1].body, requests[2].body);
    assert_eq!(
        header_of(&requests[2], "authorization").as_deref(),
        Some("Bearer second-access-token")
    );
    assert_eq!(
        header_of(&requests[2], "mcp-session-id").as_deref(),
        Some("oauth-session")
    );
}

/// The closing DELETE is refreshed too, so the session is not left open.
#[tokio::test]
async fn a_401_on_the_closing_delete_is_refreshed() {
    let server = MockServer::start().await;
    let accepted = Arc::new(Mutex::new("Bearer first-access-token".to_owned()));
    mount_server(&server, accepted.clone()).await;
    let tokens = Tokens::new(
        Some("Bearer first-access-token"),
        Some("Bearer second-access-token"),
    );
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    let client = McpClient::connect_http_with(transport).await.unwrap();
    *accepted.lock().unwrap() = "Bearer second-access-token".into();
    client.close().await.unwrap();

    let challenges = tokens.challenges.lock().unwrap().clone();
    assert_eq!(challenges.len(), 1);
    assert_eq!(challenges[0].method, "DELETE");
    let deletes: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .collect();
    assert_eq!(deletes.len(), 2);
    assert_eq!(
        header_of(&deletes[1], "authorization").as_deref(),
        Some("Bearer second-access-token")
    );
    assert_eq!(
        header_of(&deletes[1], "mcp-session-id").as_deref(),
        Some("oauth-session")
    );
}

struct FailsOnChallenge;

#[async_trait::async_trait]
impl AuthorizationSource for FailsOnChallenge {
    async fn authorization(&self) -> Result<Option<String>, McpError> {
        Ok(Some("Bearer stale-access-token".into()))
    }
    async fn challenged(&self, _: &AuthChallenge) -> Result<Option<String>, McpError> {
        Err(McpError::Transport("refresh token revoked".into()))
    }
}

/// A source that fails on the challenge fails the request with its error,
/// naming the request; nothing is resent.
#[tokio::test]
async fn a_source_failing_on_the_challenge_fails_the_request_unsent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).insert_header("WWW-Authenticate", CHALLENGE))
        .expect(1)
        .mount(&server)
        .await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(Arc::new(FailsOnChallenge))
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("tools/call", None))
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), None);
    let text = err.to_string();
    assert!(text.contains("after HTTP 401 on 'tools/call'"), "{text}");
    assert!(text.contains("refresh token revoked"), "{text}");
}

/// An invalid value from the challenge is refused unsent and unechoed.
#[tokio::test]
async fn an_invalid_refreshed_value_is_refused_unsent() {
    let server = MockServer::start().await;
    mount_server(&server, Arc::new(Mutex::new("Bearer other".into()))).await;
    let tokens = Tokens::new(
        Some("Bearer first-access-token"),
        Some("Bearer bad\nsecret-part"),
    );
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens)
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("challenged()"), "{text}");
    assert!(!text.contains("secret-part"), "{text}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// Returning the value the server just refused is not retried.
#[tokio::test]
async fn the_refused_value_again_is_not_retried() {
    let server = MockServer::start().await;
    mount_server(&server, Arc::new(Mutex::new("Bearer other".into()))).await;
    let tokens = Tokens::new(
        Some("Bearer same-access-token"),
        Some("Bearer same-access-token"),
    );
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens)
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    assert_eq!(err.http_status(), Some(401));
    assert!(!err.to_string().contains("refused again"), "{err}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// A 401 without `WWW-Authenticate`: no challenge text, the source still
/// asked; several `WWW-Authenticate` headers are all kept, joined.
#[tokio::test]
async fn challenges_absent_or_several() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let tokens = Tokens::new(Some("Bearer an-access-token"), None);
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    assert_eq!(err.www_authenticate(), None);
    assert_eq!(tokens.challenges.lock().unwrap()[0].www_authenticate, None);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401)
                .append_header("WWW-Authenticate", r#"Basic realm="mcp""#)
                .append_header("WWW-Authenticate", CHALLENGE),
        )
        .mount(&server)
        .await;
    let err = HttpTransport::new(&server.uri())
        .unwrap()
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    assert_eq!(
        err.www_authenticate(),
        Some(format!(r#"Basic realm="mcp", {CHALLENGE}"#).as_str())
    );
}

/// Concurrent requests refused together each report their refused value, so a
/// source can refresh once and answer the rest with the current token.
#[tokio::test]
async fn concurrent_refusals_each_report_the_value_they_carried() {
    let server = MockServer::start().await;
    mount_server(
        &server,
        Arc::new(Mutex::new("Bearer second-access-token".into())),
    )
    .await;
    let tokens = Tokens::new(
        Some("Bearer first-access-token"),
        Some("Bearer second-access-token"),
    );
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_authorization(tokens.clone())
        .unwrap();
    let (a, b) = tokio::join!(
        transport.send(JsonRpcRequest::new("tools/list", None)),
        transport.send(JsonRpcRequest::new("tools/list", None)),
    );
    a.unwrap();
    b.unwrap();
    let challenges = tokens.challenges.lock().unwrap().clone();
    assert!(!challenges.is_empty());
    for challenge in &challenges {
        assert_eq!(challenge.refused(), Some("Bearer first-access-token"));
    }
}

/// A token echoed without its scheme, and a fixed API key, are redacted; a
/// `User-Agent` is not a credential and stays readable.
#[tokio::test]
async fn credentials_are_redacted_and_other_headers_kept() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let token = header_of(request, "authorization").unwrap_or_default();
            let token = token.trim_start_matches("Bearer ");
            let key = header_of(request, "x-api-key").unwrap_or_default();
            let agent = header_of(request, "user-agent").unwrap_or_default();
            ResponseTemplate::new(500)
                .set_body_string(format!("token {token} key {key} agent {agent}"))
        })
        .mount(&server)
        .await;
    let transport = HttpTransport::new(&server.uri())
        .unwrap()
        .with_header("X-Api-Key", "k-1234567890")
        .unwrap()
        .with_header("User-Agent", "my-app/1.0.0")
        .unwrap()
        .with_authorization(Tokens::new(Some("Bearer bare-access-token"), None))
        .unwrap();
    let err = transport
        .send(JsonRpcRequest::new("ping", None))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(!text.contains("bare-access-token"), "{text}");
    assert!(!text.contains("k-1234567890"), "{text}");
    assert!(
        text.contains("token [redacted] key [redacted] agent my-app/1.0.0"),
        "{text}"
    );
}

/// `AuthChallenge` can be built to test a source, and never prints the
/// refused value.
#[test]
fn a_challenge_builds_and_hides_the_refused_value() {
    let challenge = AuthChallenge::new(401, "tools/call")
        .with_www_authenticate(CHALLENGE)
        .with_refused("Bearer very-secret-token");
    assert_eq!(challenge.refused(), Some("Bearer very-secret-token"));
    assert_eq!(challenge.www_authenticate.as_deref(), Some(CHALLENGE));
    let debug = format!("{challenge:?}");
    assert!(!debug.contains("very-secret-token"), "{debug}");
    assert!(debug.contains("[redacted]"), "{debug}");
}
