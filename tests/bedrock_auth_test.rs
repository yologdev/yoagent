//! Authentication of `BedrockProvider` requests (#174), against a local mock
//! server: SigV4 signing with IAM credentials, Bedrock API keys as bearer
//! tokens, an explicit `authorization` header, the environment variables,
//! and the errors raised before anything is sent.
//!
//! These tests mutate process environment variables (`AWS_*`), so they live
//! in their own binary and every test holds [`ENV_LOCK`] for its whole run.
//!
//! The SigV4 check at the bottom is written independently of the crate's
//! signer: it rebuilds the canonical request from what the server *received*
//! (method, raw path, the headers named in `SignedHeaders`, the body bytes)
//! and recomputes the signature, so it proves the signature covers the bytes
//! actually sent. The signer itself is pinned to AWS's published test vectors
//! in `src/provider/sigv4.rs`.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use yoagent::provider::{
    resolve_api_key, ApiProtocol, BedrockProvider, ModelConfig, ProviderError, StreamConfig,
    StreamProvider,
};
use yoagent::*;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

const ENV_VARS: &[&str] = &[
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "API_KEY",
    "YOAGENT_API_KEY",
];

const ACCESS: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const TOKEN: &str = "FwoGZXIvYXdzEXAMPLESESSIONTOKEN";

fn clear_env() {
    for v in ENV_VARS {
        std::env::remove_var(v);
    }
}

/// A server that records requests and answers 403, and a handle to what it
/// received. The response is irrelevant here: these tests are about the
/// request.
async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403).set_body_raw(r#"{"message":"denied"}"#, "application/json"),
        )
        .mount(&server)
        .await;
    server
}

fn stream_config(
    base_url: &str,
    model: &str,
    api_key: &str,
    headers: &[(&str, &str)],
) -> StreamConfig {
    let mut mc = ModelConfig::custom(
        ApiProtocol::BedrockConverseStream,
        "bedrock",
        base_url,
        model,
        "Test model",
    );
    for (k, v) in headers {
        mc.headers.insert(k.to_string(), v.to_string());
    }
    let mut config = StreamConfig::new(model, api_key);
    config.messages = vec![Message::user("hi")];
    config.model_config = Some(mc);
    config
}

async fn send(config: StreamConfig) -> Result<Message, ProviderError> {
    let (tx, _rx) = mpsc::unbounded_channel();
    BedrockProvider
        .stream(config, tx, CancellationToken::new())
        .await
}

async fn only_request(server: &MockServer) -> Request {
    let mut received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "expected exactly one request");
    received.pop().unwrap()
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers.get(name).map(|v| v.to_str().unwrap())
}

fn all_headers(req: &Request, name: &str) -> Vec<String> {
    req.headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect()
}

/// No header value or body byte sequence contains `secret`.
fn assert_never_sent(req: &Request, secret: &str) {
    for (name, value) in req.headers.iter() {
        assert!(
            !String::from_utf8_lossy(value.as_bytes()).contains(secret),
            "header {name} carries the secret"
        );
    }
    assert!(
        !req.body
            .windows(secret.len())
            .any(|w| w == secret.as_bytes()),
        "body carries the secret"
    );
    assert!(!req.url.as_str().contains(secret), "URL carries the secret");
}

// ---------------------------------------------------------------------------
// Independent SigV4 verification
// ---------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).unwrap();
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// RFC 3986 unreserved characters stay, every other byte is `%XX`.
fn encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

struct ParsedAuth {
    access: String,
    date: String,
    region: String,
    service: String,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_authorization(value: &str) -> ParsedAuth {
    let rest = value
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .unwrap_or_else(|| panic!("not a SigV4 authorization: {value}"));
    let mut credential = "";
    let mut signed = "";
    let mut signature = "";
    for part in rest.split(", ") {
        let (k, v) = part.split_once('=').unwrap();
        match k {
            "Credential" => credential = v,
            "SignedHeaders" => signed = v,
            "Signature" => signature = v,
            other => panic!("unexpected component {other}"),
        }
    }
    let scope: Vec<&str> = credential.split('/').collect();
    assert_eq!(scope.len(), 5, "{credential}");
    assert_eq!(scope[4], "aws4_request");
    ParsedAuth {
        access: scope[0].into(),
        date: scope[1].into(),
        region: scope[2].into(),
        service: scope[3].into(),
        signed_headers: signed.split(';').map(str::to_string).collect(),
        signature: signature.into(),
    }
}

/// The canonical URI SigV4 derives from the received path: each segment
/// encoded once more.
fn canonical_uri(received_path: &str) -> String {
    received_path
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Recompute the signature over what the server received and compare.
fn verify_sigv4(req: &Request, secret: &str) -> ParsedAuth {
    let auth = parse_authorization(header(req, "authorization").unwrap());
    let amz_date = header(req, "x-amz-date").expect("x-amz-date");
    assert_eq!(&amz_date[..8], auth.date);

    let mut sorted = auth.signed_headers.clone();
    sorted.sort();
    assert_eq!(sorted, auth.signed_headers, "SignedHeaders must be sorted");
    for required in ["host", "x-amz-date", "x-amz-content-sha256", "content-type"] {
        assert!(
            auth.signed_headers.iter().any(|h| h == required),
            "{required} is not signed"
        );
    }

    let canonical_headers: String = auth
        .signed_headers
        .iter()
        .map(|name| {
            let values = all_headers(req, name);
            assert_eq!(values.len(), 1, "header {name} sent {} times", values.len());
            format!("{name}:{}\n", values[0].trim())
        })
        .collect();
    let body_hash = hex(&Sha256::digest(&req.body));
    assert_eq!(
        header(req, "x-amz-content-sha256"),
        Some(body_hash.as_str()),
        "x-amz-content-sha256 is not the hash of the body received"
    );
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        canonical_uri(req.url.path()),
        req.url.query().unwrap_or(""),
        canonical_headers,
        auth.signed_headers.join(";"),
        body_hash
    );
    let scope = format!(
        "{}/{}/{}/aws4_request",
        auth.date, auth.region, auth.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let k = hmac_sha256(format!("AWS4{secret}").as_bytes(), &auth.date);
    let k = hmac_sha256(&k, &auth.region);
    let k = hmac_sha256(&k, &auth.service);
    let k = hmac_sha256(&k, "aws4_request");
    let expected = hex(&hmac_sha256(&k, &string_to_sign));
    assert_eq!(
        auth.signature, expected,
        "signature does not match the request received\n{canonical_request}"
    );
    auth
}

// ---------------------------------------------------------------------------
// SigV4
// ---------------------------------------------------------------------------

/// IAM credentials in `api_key`: the request is SigV4-signed for service
/// `bedrock`, carries the session token, and never contains the secret. Model
/// ids with `:` (a version suffix), a cross-region inference profile and an
/// ARN with `/` are encoded once on the wire and twice in the signed path.
#[tokio::test]
async fn iam_credentials_sign_the_request_sent() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("AWS_REGION", "us-west-2");

    for (model, wire_path) in [
        (
            "anthropic.claude-sonnet-5-v1:0",
            "/model/anthropic.claude-sonnet-5-v1%3A0/converse-stream",
        ),
        (
            "us.anthropic.claude-sonnet-5-v1:0",
            "/model/us.anthropic.claude-sonnet-5-v1%3A0/converse-stream",
        ),
        (
            "arn:aws:bedrock:us-west-2:123456789012:inference-profile/us.anthropic.claude-sonnet-5-v1:0",
            "/model/arn%3Aaws%3Abedrock%3Aus-west-2%3A123456789012%3Ainference-profile%2Fus.anthropic.claude-sonnet-5-v1%3A0/converse-stream",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(wire_path))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let key = format!("{ACCESS}:{SECRET}:{TOKEN}");
        let result = send(stream_config(&server.uri(), model, &key, &[])).await;
        assert!(result.is_err());
        let req = only_request(&server).await;
        assert_eq!(req.url.path(), wire_path);

        let auth = verify_sigv4(&req, SECRET);
        assert_eq!(auth.access, ACCESS);
        assert_eq!(auth.region, "us-west-2");
        assert_eq!(auth.service, "bedrock");
        assert_eq!(
            auth.signed_headers,
            [
                "content-type",
                "host",
                "x-amz-content-sha256",
                "x-amz-date",
                "x-amz-security-token"
            ]
        );
        assert_eq!(header(&req, "x-amz-security-token"), Some(TOKEN));
        // The signed path is the wire path encoded again: `%3A` → `%253A`.
        assert!(canonical_uri(req.url.path()).contains("%253A"));
        assert_never_sent(&req, SECRET);
        assert_eq!(all_headers(&req, "authorization").len(), 1);
        server.verify().await;
    }
}

#[tokio::test]
async fn iam_credentials_without_session_token() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("AWS_DEFAULT_REGION", "eu-central-1");
    let server = server().await;
    let key = format!("{ACCESS}:{SECRET}");
    let _ = send(stream_config(
        &server.uri(),
        "amazon.nova-pro-v1:0",
        &key,
        &[],
    ))
    .await;
    let req = only_request(&server).await;
    let auth = verify_sigv4(&req, SECRET);
    assert_eq!(auth.region, "eu-central-1");
    assert!(header(&req, "x-amz-security-token").is_none());
    assert!(!auth
        .signed_headers
        .iter()
        .any(|h| h == "x-amz-security-token"));
    assert_never_sent(&req, SECRET);
}

/// IAM credentials from the environment, with an empty `api_key` (as
/// `Agent::new(BedrockProvider)` leaves it).
#[tokio::test]
async fn iam_credentials_from_the_environment() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("AWS_ACCESS_KEY_ID", ACCESS);
    std::env::set_var("AWS_SECRET_ACCESS_KEY", SECRET);
    std::env::set_var("AWS_SESSION_TOKEN", TOKEN);
    std::env::set_var("AWS_REGION", "ap-northeast-1");
    let server = server().await;
    let _ = send(stream_config(
        &server.uri(),
        "anthropic.claude-test",
        "",
        &[],
    ))
    .await;
    let req = only_request(&server).await;
    let auth = verify_sigv4(&req, SECRET);
    assert_eq!(auth.region, "ap-northeast-1");
    assert_eq!(header(&req, "x-amz-security-token"), Some(TOKEN));
    assert_never_sent(&req, SECRET);
    clear_env();
}

/// `Agent::from_config` resolves the key from the environment and the
/// request is signed end to end.
#[tokio::test]
async fn agent_from_config_signs_with_env_credentials() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("AWS_ACCESS_KEY_ID", ACCESS);
    std::env::set_var("AWS_SECRET_ACCESS_KEY", SECRET);
    std::env::set_var("AWS_REGION", "us-east-2");
    let server = server().await;
    let mut agent = agent::Agent::from_config(ModelConfig::custom(
        ApiProtocol::BedrockConverseStream,
        "bedrock",
        server.uri(),
        "anthropic.claude-sonnet-5-v1:0",
        "Test",
    ))
    .with_retry_config(RetryConfig::none());
    let mut rx = agent.prompt("hi").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let req = only_request(&server).await;
    let auth = verify_sigv4(&req, SECRET);
    assert_eq!(auth.region, "us-east-2");
    assert_never_sent(&req, SECRET);
    clear_env();
}

// ---------------------------------------------------------------------------
// Bearer tokens and explicit headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bedrock_api_key_is_a_bearer_token() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    let server = server().await;
    let _ = send(stream_config(
        &server.uri(),
        "anthropic.claude-test",
        "bedrock-api-key-QUJDREVGRw==",
        &[],
    ))
    .await;
    let req = only_request(&server).await;
    assert_eq!(
        all_headers(&req, "authorization"),
        ["Bearer bedrock-api-key-QUJDREVGRw=="]
    );
    assert!(header(&req, "x-amz-date").is_none());
}

/// `AWS_BEARER_TOKEN_BEDROCK` wins over IAM credentials in the environment,
/// both when the provider reads it and in `resolve_api_key`.
#[tokio::test]
async fn bearer_token_env_var_wins_over_iam_env_credentials() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", "env-bedrock-key");
    std::env::set_var("AWS_ACCESS_KEY_ID", ACCESS);
    std::env::set_var("AWS_SECRET_ACCESS_KEY", SECRET);
    assert_eq!(
        resolve_api_key("bedrock").as_deref(),
        Some("env-bedrock-key")
    );
    let server = server().await;
    let _ = send(stream_config(
        &server.uri(),
        "anthropic.claude-test",
        "",
        &[],
    ))
    .await;
    let req = only_request(&server).await;
    assert_eq!(
        all_headers(&req, "authorization"),
        ["Bearer env-bedrock-key"]
    );
    assert_never_sent(&req, SECRET);

    // Without it, IAM credentials compose as before.
    std::env::remove_var("AWS_BEARER_TOKEN_BEDROCK");
    assert_eq!(
        resolve_api_key("bedrock"),
        Some(format!("{ACCESS}:{SECRET}"))
    );
    std::env::set_var("AWS_SESSION_TOKEN", TOKEN);
    assert_eq!(
        resolve_api_key("bedrock"),
        Some(format!("{ACCESS}:{SECRET}:{TOKEN}"))
    );
    clear_env();
    assert_eq!(resolve_api_key("bedrock"), None);
}

/// An `authorization` header in `ModelConfig.headers` (any case) is sent
/// alone: no bearer, no signature, even with IAM credentials present.
#[tokio::test]
async fn explicit_authorization_header_wins() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("AWS_REGION", "us-east-1");
    for name in ["authorization", "Authorization"] {
        let server = server().await;
        let key = format!("{ACCESS}:{SECRET}");
        let _ = send(stream_config(
            &server.uri(),
            "anthropic.claude-test",
            &key,
            &[(name, "AWS4-HMAC-SHA256 precomputed")],
        ))
        .await;
        let req = only_request(&server).await;
        assert_eq!(
            all_headers(&req, "authorization"),
            ["AWS4-HMAC-SHA256 precomputed"]
        );
        assert!(header(&req, "x-amz-date").is_none());
        assert_never_sent(&req, SECRET);
    }
    clear_env();
}

// ---------------------------------------------------------------------------
// Errors before sending
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_credentials_is_an_error_and_nothing_is_sent() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let err = send(stream_config(
        &server.uri(),
        "anthropic.claude-test",
        "",
        &[],
    ))
    .await
    .unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
    server.verify().await;
}

/// SigV4 needs a region; a non-AWS host with no `AWS_REGION` is an error,
/// not a request signed for a guessed region.
#[tokio::test]
async fn sigv4_without_a_region_is_an_error_and_nothing_is_sent() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let key = format!("{ACCESS}:{SECRET}");
    let err = send(stream_config(
        &server.uri(),
        "anthropic.claude-test",
        &key,
        &[],
    ))
    .await
    .unwrap_err();
    match &err {
        ProviderError::Auth(m) => {
            assert!(m.contains("AWS_REGION"), "{m}");
            assert!(!m.contains(SECRET));
        }
        other => panic!("{other:?}"),
    }
    server.verify().await;
}

/// A malformed `access:secret` value is refused without echoing it.
#[tokio::test]
async fn malformed_iam_key_is_an_error_and_nothing_is_sent() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let err = send(stream_config(
        &server.uri(),
        "anthropic.claude-test",
        ":secret-only",
        &[],
    ))
    .await
    .unwrap_err();
    match &err {
        ProviderError::Auth(m) => assert!(!m.contains("secret-only"), "{m}"),
        other => panic!("{other:?}"),
    }
    server.verify().await;
}

// ---------------------------------------------------------------------------
// Review round (#174)
// ---------------------------------------------------------------------------

/// A server that must receive nothing.
async fn silent_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    server
}

async fn run_agent(config: ModelConfig) -> agent::Agent {
    let mut agent = agent::Agent::from_config(config).with_retry_config(RetryConfig::none());
    let mut rx = agent.prompt("hi").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    agent
}

fn custom(base_url: &str, provider: &str) -> ModelConfig {
    ModelConfig::custom(
        ApiProtocol::BedrockConverseStream,
        provider,
        base_url,
        "anthropic.claude-sonnet-5-v1:0",
        "Test",
    )
}

/// Key resolution follows the protocol, not the provider string: a Bedrock
/// config named "aws-bedrock" must not send the generic `API_KEY` to AWS as
/// a bearer token, and must use the AWS credentials in the environment.
#[tokio::test]
async fn bedrock_protocol_ignores_generic_api_key_env_vars() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("API_KEY", "unrelated-generic-key");
    std::env::set_var("YOAGENT_API_KEY", "unrelated-yoagent-key");
    std::env::set_var("AWS_ACCESS_KEY_ID", ACCESS);
    std::env::set_var("AWS_SECRET_ACCESS_KEY", SECRET);
    std::env::set_var("AWS_REGION", "us-east-1");
    let server = server().await;
    run_agent(custom(&server.uri(), "aws-bedrock")).await;
    let req = only_request(&server).await;
    let auth = verify_sigv4(&req, SECRET);
    assert_eq!(auth.access, ACCESS);
    assert_never_sent(&req, "unrelated-generic-key");
    assert_never_sent(&req, "unrelated-yoagent-key");
    assert_never_sent(&req, SECRET);

    // With no AWS credentials, the generic key is still not used: nothing
    // is sent.
    std::env::remove_var("AWS_ACCESS_KEY_ID");
    std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    let silent = silent_server().await;
    let agent = run_agent(custom(&silent.uri(), "aws-bedrock")).await;
    silent.verify().await;
    let last = agent.messages().last().cloned();
    let text = format!("{last:?}");
    assert!(text.contains("no Amazon Bedrock credentials"), "{text}");
    clear_env();
}

/// A trailing slash on `base_url` must not produce `//model/…`, which AWS
/// normalizes before checking the signature.
#[tokio::test]
async fn trailing_slash_on_base_url_is_trimmed() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("AWS_REGION", "us-east-1");
    let server = server().await;
    let key = format!("{ACCESS}:{SECRET}");
    let base = format!("{}//", server.uri());
    let _ = send(stream_config(&base, "anthropic.claude-test", &key, &[])).await;
    let req = only_request(&server).await;
    assert_eq!(
        req.url.path(),
        "/model/anthropic.claude-test/converse-stream"
    );
    verify_sigv4(&req, SECRET);
    clear_env();
}

/// IAM credentials joined with a space are refused, not sent as a bearer.
#[tokio::test]
async fn malformed_keys_send_nothing() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    for key in [
        format!("{ACCESS} {SECRET}"),
        format!("AKIAIOSFODNN7EXAMPLE;{SECRET}"),
        SECRET.to_string(),
        format!("ASIAIOSFODNN7EXAMPLE:{SECRET}"),
        format!("{ACCESS}:{SECRET}:tok\nen"),
    ] {
        std::env::set_var("AWS_REGION", "us-east-1");
        let server = silent_server().await;
        let err = send(stream_config(
            &server.uri(),
            "anthropic.claude-test",
            &key,
            &[],
        ))
        .await
        .unwrap_err();
        match &err {
            ProviderError::Auth(m) => assert!(!m.contains(SECRET), "{m}"),
            other => panic!("{key:?}: {other:?}"),
        }
        server.verify().await;
    }
    clear_env();
}

/// A user header that SigV4 sets itself would be sent twice: refused before
/// sending.
#[tokio::test]
async fn colliding_user_header_on_the_sigv4_path_sends_nothing() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    std::env::set_var("AWS_REGION", "us-east-1");
    let server = silent_server().await;
    let key = format!("{ACCESS}:{SECRET}");
    let err = send(stream_config(
        &server.uri(),
        "anthropic.claude-test",
        &key,
        &[("Content-Type", "application/json")],
    ))
    .await
    .unwrap_err();
    assert!(
        matches!(&err, ProviderError::Auth(m) if m.contains("`Content-Type`")),
        "{err:?}"
    );
    server.verify().await;
    clear_env();
}

/// AWS's clock-skew rejection gets a hint to check the system clock.
#[tokio::test]
async fn signature_expired_suggests_checking_the_clock() {
    let _guard = ENV_LOCK.lock().await;
    clear_env();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_raw(
            r#"{"message":"Signature expired: 20260928T000000Z is now earlier than 20260928T000600Z (20260928T001100Z - 5 min.)"}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let err = send(stream_config(
        &server.uri(),
        "anthropic.claude-test",
        "bedrock-api-key-QUJD",
        &[],
    ))
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("check the system clock"), "{err}");
}
