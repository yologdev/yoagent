//! Amazon Bedrock ConverseStream provider.
//!
//! **Authentication**, first match wins:
//!
//! 1. An `authorization` entry in `ModelConfig.headers` (any case) is sent as
//!    given and nothing else is added — for pre-computed auth or a signing
//!    proxy.
//! 2. An explicit `api_key` (`with_api_key`): `access_key_id:secret_access_key`
//!    (with `:session_token`, required for temporary `ASIA…` keys) is IAM
//!    credentials and the request is SigV4-signed; a value without `:` is a
//!    Bedrock API key, sent as `Authorization: Bearer <key>` — unless it is
//!    evidently not one (whitespace or control characters, an `AKIA`/`ASIA`
//!    start, characters outside base64, or the length of a bare secret key),
//!    which is refused so IAM credentials never go out as a bearer token.
//! 3. With an empty `api_key` — which is what `Agent`, `SubAgentTool` and
//!    `LlmCompaction` pass for any `BedrockConverseStream` config — the
//!    environment when the request is built: `AWS_BEARER_TOKEN_BEDROCK`
//!    (bearer, same checks), then `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`
//!    (+ `AWS_SESSION_TOKEN`) (SigV4).
//!
//! Every failure — no credentials, half a pair, a non-Unicode variable, a
//! value that cannot be an HTTP header, no region — is a
//! [`ProviderError::Auth`] before anything is sent, and no message contains a
//! credential. The secret access key is never sent: SigV4 sends only the
//! access key id and an HMAC signature. The SigV4 signing region is the one in
//! the endpoint host (`bedrock-runtime[-fips].<region>.` followed by an AWS
//! partition's DNS suffix, dual-stack `api.aws` included, or a VPC endpoint
//! host), else `AWS_REGION`, else `AWS_DEFAULT_REGION`; the signing name is
//! `bedrock`. The model id is percent-encoded in the path (`:` becomes `%3A`,
//! `/` in an ARN `%2F`), as the AWS SDKs send it.
//!
//! The `base_url` in ModelConfig should be the Bedrock endpoint, e.g.
//! `https://bedrock-runtime.us-east-1.amazonaws.com`.
//!
//! **Response stream.** ConverseStream answers with binary
//! `application/vnd.amazon.eventstream` frames (decoded by the crate-private
//! `provider::eventstream` module), not JSON lines. The event type is
//! the `:event-type` header of each frame, and the JSON payload is the event
//! structure itself — `{"contentBlockIndex":0,"delta":{"text":"Hi"}}` for a
//! `contentBlockDelta`, with no wrapper key. The shapes below follow the AWS
//! Bedrock Runtime API reference (`ConverseStreamOutput` and the event types
//! it lists). Tested against mock frames built to that format, not against a
//! live endpoint.

use super::eventstream::{Frame, FrameDecoder, FrameError};
use super::sigv4::{self, Credentials};
use super::tool_args::finalize_tool_arguments;
use super::traits::*;
use crate::provider::UNPARSED_ARGUMENTS_KEY;
use crate::types::*;
use futures::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct BedrockProvider;

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl StreamProvider for BedrockProvider {
    fn protocol(&self) -> Option<crate::provider::ApiProtocol> {
        Some(crate::provider::ApiProtocol::BedrockConverseStream)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        if config.output_schema.is_some() {
            tracing::warn!(
                "structured outputs are not yet wired for the Amazon Bedrock provider; output_schema will be ignored"
            );
        }
        let model_config = config
            .model_config
            .as_ref()
            .ok_or_else(|| ProviderError::Other("ModelConfig required".into()))?;

        let env = |name: &str| std::env::var(name);
        let auth = resolve_auth(&config.api_key, &model_config.headers, &env)?;

        // A trailing slash would make `//model/…`, which AWS normalizes to
        // `/model/…` before checking the signature.
        let url = format!(
            "{}/model/{}/converse-stream",
            model_config.base_url.trim_end_matches('/'),
            sigv4::uri_encode(&config.model)
        );
        let parsed_url = reqwest::Url::parse(&url)
            .map_err(|e| ProviderError::Other(format!("invalid Bedrock URL `{url}`: {e}")))?;

        // Serialize once: these exact bytes are hashed for SigV4 and sent.
        let body = serde_json::to_vec(&build_bedrock_body(&config))
            .map_err(|e| ProviderError::Other(format!("failed to encode request: {e}")))?;
        debug!("Bedrock request: model={} url={}", config.model, url);

        let headers = request_headers(
            &parsed_url,
            &body,
            auth,
            &model_config.headers,
            &env,
            sigv4::amz_date_now,
        )?;

        let request = reqwest::Client::new()
            .post(parsed_url)
            .headers(headers)
            .body(body);

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            r = request.send() => r.map_err(send_error)?,
        };

        if !response.status().is_success() {
            return Err(http_error(response).await);
        }

        // A 200 that declares some other content type (JSON from a proxy, an
        // HTML error page, an endpoint that is not ConverseStream) is not an
        // event stream. Report its body rather than a checksum error from
        // trying to frame it. With no content type at all, try to decode.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).to_ascii_lowercase());
        if let Some(content_type) = content_type {
            if !content_type.contains("application/vnd.amazon.eventstream") {
                let body = read_body(response).await;
                return Err(ProviderError::Api(format!(
                    "Bedrock returned `{content_type}` instead of an event stream: {}",
                    truncate_for_error(&body)
                )));
            }
        }

        let _ = tx.send(StreamEvent::Start);

        let outcome = read_converse_stream(response.bytes_stream(), &tx, &cancel).await?;

        let message = Message::Assistant {
            content: outcome.content,
            stop_reason: outcome.stop_reason,
            model: config.model.clone(),
            provider: model_config.provider.clone(),
            usage: outcome.usage,
            timestamp: now_ms(),
            error_message: outcome.error_message,
        };

        let _ = tx.send(StreamEvent::Done {
            message: message.clone(),
        });
        Ok(message)
    }
}

/// The environment variable AWS documents for a Bedrock API key.
pub(crate) const BEARER_TOKEN_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";

/// The SigV4 signing name of Bedrock Runtime (`signingName` in the service
/// model; the endpoint prefix `bedrock-runtime` is not the signing name).
const SIGNING_NAME: &str = "bedrock";

/// Reads one environment variable, shaped like [`std::env::var`] so tests can
/// pass a fake.
type Env<'a> = &'a dyn Fn(&str) -> Result<String, std::env::VarError>;

/// How a request is authenticated. `Debug` never prints a secret.
enum Auth {
    /// `ModelConfig.headers` carries `authorization`; add nothing.
    Explicit,
    /// A Bedrock API key.
    Bearer(String),
    /// IAM credentials: sign with SigV4.
    SigV4(Credentials),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Explicit => f.write_str("Explicit"),
            Auth::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Auth::SigV4(c) => f.debug_tuple("SigV4").field(c).finish(),
        }
    }
}

const NO_CREDENTIALS: &str = "no Amazon Bedrock credentials: set AWS_BEARER_TOKEN_BEDROCK \
     (a Bedrock API key) or AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY (+ AWS_SESSION_TOKEN), \
     call .with_api_key(...) with an API key or \"access_key_id:secret_access_key[:session_token]\", \
     or put an `authorization` header in ModelConfig.headers";

/// Decide how to authenticate (see the module docs for the order). The
/// environment is consulted only when `api_key` is empty.
fn resolve_auth(
    api_key: &str,
    headers: &HashMap<String, String>,
    env: Env<'_>,
) -> Result<Auth, ProviderError> {
    if headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("authorization"))
    {
        return Ok(Auth::Explicit);
    }
    let api_key = api_key.trim();
    if !api_key.is_empty() {
        return parse_api_key(api_key);
    }
    if let Some(token) = env_value(env, BEARER_TOKEN_ENV)? {
        return Ok(Auth::Bearer(checked_bearer(token, BEARER_TOKEN_ENV)?));
    }
    let access = env_value(env, "AWS_ACCESS_KEY_ID")?;
    let secret = env_value(env, "AWS_SECRET_ACCESS_KEY")?;
    match (access, secret) {
        (Some(access_key_id), Some(secret_access_key)) => {
            let session_token = env_value(env, "AWS_SESSION_TOKEN")?;
            if session_token.is_none() && is_temporary_key_id(&access_key_id) {
                return Err(ProviderError::Auth(
                    "AWS_ACCESS_KEY_ID is a temporary (ASIA…) access key id, which needs \
                     its session token: set AWS_SESSION_TOKEN"
                        .into(),
                ));
            }
            Ok(Auth::SigV4(Credentials {
                access_key_id,
                secret_access_key,
                session_token,
            }))
        }
        (Some(_), None) => Err(ProviderError::Auth(
            "AWS_ACCESS_KEY_ID is set but AWS_SECRET_ACCESS_KEY is missing or empty".into(),
        )),
        (None, Some(_)) => Err(ProviderError::Auth(
            "AWS_SECRET_ACCESS_KEY is set but AWS_ACCESS_KEY_ID is missing or empty".into(),
        )),
        (None, None) => Err(ProviderError::Auth(NO_CREDENTIALS.into())),
    }
}

/// A set, non-blank environment variable, trimmed. Blank counts as unset; a
/// value that is not Unicode is an error rather than silently unset.
fn env_value(env: Env<'_>, name: &str) -> Result<Option<String>, ProviderError> {
    match env(name) {
        Ok(v) => {
            let v = v.trim();
            Ok((!v.is_empty()).then(|| v.to_string()))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ProviderError::Auth(format!(
            "{name} is set but is not valid Unicode"
        ))),
    }
}

/// Interpret an explicit `api_key`: `access:secret[:token]` is IAM
/// credentials, anything without `:` a Bedrock API key (checked by
/// [`checked_bearer`]).
fn parse_api_key(api_key: &str) -> Result<Auth, ProviderError> {
    if api_key.contains(':') {
        let mut parts = api_key.splitn(3, ':');
        let access_key_id = parts.next().unwrap_or_default().trim();
        let secret_access_key = parts.next().unwrap_or_default().trim();
        let session_token = parts
            .next()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        if access_key_id.is_empty() || secret_access_key.is_empty() {
            return Err(ProviderError::Auth(
                "Bedrock api_key with `:` must be \
                 'access_key_id:secret_access_key[:session_token]' with both keys non-empty"
                    .into(),
            ));
        }
        if session_token.is_none() && is_temporary_key_id(access_key_id) {
            return Err(ProviderError::Auth(
                "Bedrock api_key has a temporary (ASIA…) access key id but no session \
                 token: pass 'access_key_id:secret_access_key:session_token' \
                 (the :session_token is required)"
                    .into(),
            ));
        }
        return Ok(Auth::SigV4(Credentials {
            access_key_id: access_key_id.to_string(),
            secret_access_key: secret_access_key.to_string(),
            session_token,
        }));
    }
    Ok(Auth::Bearer(checked_bearer(
        api_key.to_string(),
        "api_key",
    )?))
}

/// Temporary (STS) credentials have an `ASIA` access key id.
fn is_temporary_key_id(access_key_id: &str) -> bool {
    access_key_id.starts_with("ASIA")
}

/// The prefixes of Bedrock API keys: `bedrock-api-key-` for short-term keys
/// (AWS's token generators), `ABSK` for long-term keys (observed, not in
/// AWS's documentation — so a missing prefix only warns).
const API_KEY_PREFIXES: [&str; 2] = ["bedrock-api-key-", "ABSK"];

/// Refuse a bearer candidate that is evidently not a Bedrock API key — above
/// all IAM credentials joined with something other than `:`, which would
/// otherwise go out in a bearer header. Bedrock API keys are base64 text
/// (`bedrock-api-key-` + base64 for short-term keys). Messages name `source`,
/// never the value.
fn checked_bearer(key: String, source: &str) -> Result<String, ProviderError> {
    let refuse = |why: &str| {
        Err(ProviderError::Auth(format!(
            "{source} is not a Bedrock API key ({why}); for IAM credentials use \
             'access_key_id:secret_access_key[:session_token]'"
        )))
    };
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return refuse("it contains whitespace or control characters");
    }
    if key.starts_with("AKIA") || key.starts_with("ASIA") {
        return refuse("it starts like an AWS access key id");
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'=' | b'/' | b'+'))
    {
        return refuse("it contains characters that do not occur in one");
    }
    let known_prefix = API_KEY_PREFIXES.iter().any(|p| key.starts_with(p));
    if !known_prefix && key.len() == 40 {
        return refuse("it has the length of a secret access key");
    }
    if !known_prefix {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            warn!(
                "{source} does not start with `bedrock-api-key-` or `ABSK`; sending it as a \
                 Bedrock API key (bearer token) anyway"
            )
        });
    }
    Ok(key)
}

/// The SigV4 region: from the endpoint host when it is a Bedrock Runtime
/// host, else `AWS_REGION`, else `AWS_DEFAULT_REGION`.
fn signing_region(host: &str, env: Env<'_>) -> Result<String, ProviderError> {
    if let Some(region) = region_from_host(host) {
        return Ok(region);
    }
    for name in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Some(region) = env_value(env, name)? {
            return Ok(region);
        }
    }
    Err(ProviderError::Auth(format!(
        "cannot determine the AWS region for SigV4: the endpoint host `{host}` \
         is not bedrock-runtime.<region>.amazonaws.com; set AWS_REGION"
    )))
}

/// The DNS suffixes that follow `bedrock-runtime[-fips].<region>` in AWS's
/// endpoint rules (botocore `bedrock-runtime` endpoint-rule-set and
/// `partitions.json`: the `dnsSuffix` and `dualStackDnsSuffix` of the aws,
/// aws-us-gov, aws-cn and aws-eusc partitions; the isolated ISO partitions are
/// not listed, so their hosts fall back to `AWS_REGION`),
/// plus the VPC endpoint forms.
const AWS_DNS_SUFFIXES: [&str; 8] = [
    "amazonaws.com",
    "api.aws",
    "amazonaws.com.cn",
    "api.amazonwebservices.com.cn",
    "amazonaws.eu",
    "api.amazonwebservices.eu",
    "vpce.amazonaws.com",
    "vpce.amazonaws.com.cn",
];

/// `bedrock-runtime[-fips].<region>.<suffix>` for a suffix in
/// [`AWS_DNS_SUFFIXES`] (VPC endpoint hosts have more labels in front): the
/// label after `bedrock-runtime`.
fn region_from_host(host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let at = labels
        .iter()
        .position(|l| *l == "bedrock-runtime" || *l == "bedrock-runtime-fips")?;
    let region = labels.get(at + 1)?;
    let suffix = labels.get(at + 2..)?.join(".");
    let valid = region
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && region.bytes().any(|b| b.is_ascii_digit());
    (valid && AWS_DNS_SUFFIXES.contains(&suffix.as_str())).then(|| region.to_string())
}

/// Headers the SigV4 path sets and signs; a `ModelConfig.headers` entry with
/// one of these names would be sent twice (reqwest appends) and break the
/// signature.
const SIGNED_HEADER_NAMES: [&str; 5] = [
    "host",
    "content-type",
    "x-amz-date",
    "x-amz-security-token",
    "x-amz-content-sha256",
];

/// Every header of the request: `content-type`, `accept`, then
/// `ModelConfig.headers`, then the authentication headers. Every value is
/// validated here, so a newline in a key is an error naming the header
/// instead of a transport "builder error" that would be retried.
fn request_headers(
    url: &reqwest::Url,
    body: &[u8],
    auth: Auth,
    extra: &HashMap<String, String>,
    env: Env<'_>,
    now: fn() -> Result<String, String>,
) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.amazon.eventstream"),
    );

    if matches!(auth, Auth::SigV4(_)) {
        if let Some(name) = extra.keys().find(|k| {
            SIGNED_HEADER_NAMES
                .iter()
                .any(|s| k.eq_ignore_ascii_case(s))
        }) {
            return Err(ProviderError::Auth(format!(
                "ModelConfig.headers sets `{name}`, which SigV4 signing sets itself; \
                 remove it (or supply your own `authorization` header)"
            )));
        }
    }
    for (name, value) in extra {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderError::Other(format!(
                "invalid header name `{name}` in ModelConfig.headers"
            ))
        })?;
        let is_auth = header_name == AUTHORIZATION || header_name == "x-amz-security-token";
        let value = header_value(value, name, is_auth).map_err(|e| {
            if is_auth {
                e
            } else {
                ProviderError::Other(format!(
                    "invalid value for header `{name}` in ModelConfig.headers"
                ))
            }
        })?;
        headers.append(header_name, value);
    }

    match auth {
        Auth::Explicit => {}
        Auth::Bearer(token) => {
            headers.insert(
                AUTHORIZATION,
                header_value(&format!("Bearer {token}"), "authorization", true)?,
            );
        }
        Auth::SigV4(credentials) => {
            let amz_date = now().map_err(ProviderError::Auth)?;
            for (name, value) in sigv4_headers(url, body, &credentials, &amz_date, env)? {
                let sensitive = name == "authorization" || name == "x-amz-security-token";
                let value = header_value(&value, &name, sensitive)?;
                let name =
                    HeaderName::from_bytes(name.as_bytes()).expect("SigV4 header names are valid");
                headers.insert(name, value);
            }
        }
    }
    Ok(headers)
}

/// A header value, or an `Auth` error naming the header (never the value).
/// Credential-bearing values are marked sensitive so they are redacted from
/// `Debug` output.
/// Credential values must also be ASCII: `HeaderValue` accepts raw UTF-8
/// bytes, which no AWS credential contains.
fn header_value(value: &str, name: &str, sensitive: bool) -> Result<HeaderValue, ProviderError> {
    let invalid = || {
        ProviderError::Auth(format!(
            "the `{name}` header value is not valid in an HTTP header (non-ASCII or \
             control characters, such as a newline, in the credentials?)"
        ))
    };
    if sensitive && !value.is_ascii() {
        return Err(invalid());
    }
    let mut v = HeaderValue::from_str(value).map_err(|_| invalid())?;
    v.set_sensitive(sensitive);
    Ok(v)
}

/// A `send()` failure. A request reqwest could not even build is a
/// configuration error, not a retryable network failure.
fn send_error(e: reqwest::Error) -> ProviderError {
    if e.is_builder() {
        ProviderError::Other(format!("could not build the Bedrock request: {e}"))
    } else {
        ProviderError::Network(e.to_string())
    }
}

/// The SigV4 headers (`x-amz-date`, `x-amz-security-token` with a session
/// token, `x-amz-content-sha256`, `authorization`) for a POST of `body` to
/// `url`. Signs `host` and `content-type`; other headers are sent unsigned.
fn sigv4_headers(
    url: &reqwest::Url,
    body: &[u8],
    credentials: &Credentials,
    amz_date: &str,
    env: Env<'_>,
) -> Result<Vec<(String, String)>, ProviderError> {
    let host = url
        .host_str()
        .ok_or_else(|| ProviderError::Other(format!("Bedrock URL `{url}` has no host")))?;
    let region = signing_region(host, env)?;
    // As reqwest writes the Host header: the port only when not the default.
    let host_header = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let signed = sigv4::sign(
        "POST",
        url.path(),
        &[("host", &host_header), ("content-type", "application/json")],
        body,
        &sigv4::SigningParams {
            credentials,
            region: &region,
            service: SIGNING_NAME,
            amz_date,
            sign_content_sha256: true,
        },
    );
    Ok(signed.headers)
}

/// Classify a non-2xx ConverseStream response. The body is AWS's JSON error
/// (`{"message": "..."}`) and the error name is in `x-amzn-ErrorType`
/// (`ThrottlingException:http://...`). A 429 is a retryable rate limit; a
/// validation error whose message is a context-overflow phrase ("Input is too
/// long for requested model") is an overflow.
async fn http_error(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let retry_after_ms = parse_retry_after(response.headers());
    let kind = response
        .headers()
        .get("x-amzn-errortype")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(':').next())
        .map(str::to_string);
    let body = read_body(response).await;
    let mut message = error_message_of(body.as_bytes());
    if message.contains("Signature expired") || message.contains("Signature not yet current") {
        message.push_str(
            " (check the system clock: AWS rejects SigV4 signatures more than 5 minutes \
             from its own time)",
        );
    }
    let text = match kind {
        Some(kind) => format!("Bedrock error {status} ({kind}): {message}"),
        None => format!("Bedrock error {status}: {message}"),
    };
    ProviderError::classify_with_retry_after(status.as_u16(), &text, retry_after_ms)
}

/// The response body as text; a failure to read it is reported in its place
/// rather than turning into an empty message.
async fn read_body(response: reqwest::Response) -> String {
    match response.text().await {
        Ok(body) => body,
        Err(e) => format!("<failed to read the response body: {e}>"),
    }
}

/// The `message` of an AWS JSON error body, or the body itself when it has
/// none (or is not JSON).
fn error_message_of(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            ["message", "Message"]
                .iter()
                .find_map(|k| v.get(*k).and_then(|m| m.as_str()).map(str::to_string))
        })
        .unwrap_or_else(|| text.into_owned())
}

/// Map an in-stream exception (`:message-type: exception`) onto a
/// [`ProviderError`] through the HTTP status AWS documents for it in
/// `ConverseStreamOutput`, so it classifies exactly like the same error
/// returned as an HTTP response: `throttlingException` (429) is a retryable
/// rate limit, `validationException` (400) carrying an overflow phrase is a
/// context overflow, the rest are API errors.
fn exception_error(kind: &str, message: &str) -> ProviderError {
    let status = match kind.to_ascii_lowercase().as_str() {
        "throttlingexception" => 429,
        "validationexception" => 400,
        "accessdeniedexception" => 403,
        "modelstreamerrorexception" | "modelerrorexception" => 424,
        // The 5xx exceptions stay `Api` (not retried), as an HTTP 5xx does
        // everywhere else in this crate: only `RateLimited` and `Network`
        // are retryable.
        "internalserverexception" => 500,
        "serviceunavailableexception" => 503,
        _ => 0,
    };
    ProviderError::classify(status, &format!("Bedrock {kind}: {message}"))
}

fn truncate_for_error(s: &str) -> String {
    const MAX: usize = 300;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}\u{2026} ({} bytes total)", s.len())
}

/// What one ConverseStream response assembled into.
struct StreamOutcome {
    content: Vec<Content>,
    usage: Usage,
    stop_reason: StopReason,
    error_message: Option<String>,
}

/// Drive the frame decoder and the event state machine over a byte stream.
///
/// Generic over the chunk and error types so tests can feed arbitrary chunk
/// boundaries; the provider passes `reqwest`'s `bytes_stream()`.
async fn read_converse_stream<S, B, E>(
    stream: S,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<StreamOutcome, ProviderError>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut stream = std::pin::pin!(stream);
    let mut decoder = FrameDecoder::new();
    let mut state = ConverseStreamState::default();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            chunk = stream.next() => chunk,
        };
        match chunk {
            None => break,
            Some(Err(e)) => {
                // Once `messageStop` and `metadata` have both arrived the
                // response is whole — `metadata` is the last event AWS sends.
                // Failing now would make the turn retryable and re-bill a
                // finished response (the rule `classify_eventsource_error`
                // documents for SSE providers). Before that, a dropped
                // connection is a transport failure and the partial content
                // is discarded.
                if state.is_complete() {
                    warn!(
                        "Bedrock stream transport error after a complete response; keeping it: {e}"
                    );
                    return state.finish(tx);
                }
                warn!("Bedrock stream transport error: {e}");
                return Err(ProviderError::Network(format!(
                    "Bedrock stream interrupted: {e}"
                )));
            }
            Some(Ok(bytes)) => {
                decoder.push(bytes.as_ref());
                while let Some(frame) = decoder.next_frame().map_err(frame_error)? {
                    state.handle_frame(&frame, tx)?;
                }
            }
        }
    }
    if let Err(e) = decoder.finish() {
        // Same rule for a body that ends inside a trailing frame.
        if state.is_complete() && matches!(e, FrameError::Truncated { .. }) {
            warn!("Bedrock stream: {e} after a complete response; keeping it");
        } else {
            return Err(frame_error(e));
        }
    }
    state.finish(tx)
}

/// A body that ends inside a frame is truncation (retryable, like any other
/// dropped stream); a checksum or structure error means the body is not an
/// intact event stream, which retrying the same endpoint will not fix.
fn frame_error(e: FrameError) -> ProviderError {
    warn!("Bedrock event stream: {e}");
    match e {
        FrameError::Truncated { .. } => ProviderError::Network(e.to_string()),
        _ => ProviderError::Other(format!("Bedrock {e}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
    /// A block this provider does not surface (image, server-side tool use
    /// and its result); its deltas are skipped.
    Ignored,
}

#[derive(Debug)]
struct Block {
    kind: BlockKind,
    /// Position in `content` (unused for `Ignored`).
    content_index: usize,
    /// Tool blocks: the name, and the `toolUse.input` text accumulated so far.
    name: String,
    input: String,
    /// Thinking blocks: decoded `redactedContent` bytes accumulated so far.
    redacted: Vec<u8>,
    closed: bool,
}

impl Block {
    fn new(kind: BlockKind, content_index: usize, name: String) -> Self {
        Block {
            kind,
            content_index,
            name,
            input: String::new(),
            redacted: Vec::new(),
            closed: false,
        }
    }
}

/// Accumulates one ConverseStream response, keyed by `contentBlockIndex`.
///
/// Tool calls are pushed into `content` with the unparsed-arguments marker
/// (`{"__partial_json": ""}`) as a placeholder, which the agent loop never
/// runs; only `contentBlockStop` replaces it with the parsed arguments. A
/// path that skips finalization therefore fails closed instead of running
/// the tool on `{}`.
#[derive(Default)]
struct ConverseStreamState {
    content: Vec<Content>,
    blocks: BTreeMap<u64, Block>,
    stop: Option<(StopReason, Option<String>)>,
    usage: Option<Usage>,
    events: usize,
    /// Keys of warnings already logged, so dropped content warns once per
    /// block (or per unknown event type), not once per delta.
    warned: BTreeSet<String>,
}

fn parse_payload<'a, T: Deserialize<'a>>(
    event: &str,
    payload: &'a [u8],
) -> Result<T, ProviderError> {
    serde_json::from_slice(payload).map_err(|e| {
        ProviderError::Other(format!(
            "Bedrock `{event}` payload does not match the documented shape ({e}): {}",
            truncate_for_error(&String::from_utf8_lossy(payload))
        ))
    })
}

fn protocol_error(msg: String) -> ProviderError {
    warn!("Bedrock stream: {msg}");
    ProviderError::Other(format!("Bedrock stream: {msg}"))
}

fn unfinalized_arguments(raw: &str) -> serde_json::Value {
    serde_json::json!({ UNPARSED_ARGUMENTS_KEY: raw })
}

fn member_names(other: &BTreeMap<String, serde_json::Value>) -> String {
    other.keys().cloned().collect::<Vec<_>>().join(", ")
}

impl ConverseStreamState {
    /// `messageStop` and `metadata` both arrived: nothing else is expected.
    fn is_complete(&self) -> bool {
        self.stop.is_some() && self.usage.is_some()
    }

    /// Log `message` once per `key`.
    fn warn_once(&mut self, key: String, message: impl FnOnce() -> String) {
        if self.warned.insert(key) {
            warn!("Bedrock: {}", message());
        }
    }

    fn handle_frame(
        &mut self,
        frame: &Frame,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let message_type = frame
            .header_str(":message-type")
            .ok_or_else(|| protocol_error("frame without a `:message-type` header".into()))?;
        match message_type {
            "event" => {
                let event = frame.header_str(":event-type").ok_or_else(|| {
                    protocol_error("event frame without an `:event-type` header".into())
                })?;
                self.events += 1;
                self.handle_event(event, &frame.payload, tx)
            }
            // A modeled error (`throttlingException`, `validationException`,
            // ...): the name is in `:exception-type`, the payload is
            // `{"message": "..."}`.
            "exception" => {
                let kind = frame.header_str(":exception-type").unwrap_or("exception");
                let err = exception_error(kind, &error_message_of(&frame.payload));
                warn!("Bedrock stream exception: {err}");
                Err(err)
            }
            // An unmodeled error: name and text travel in headers.
            "error" => {
                let code = frame.header_str(":error-code").unwrap_or("error");
                let message = frame.header_str(":error-message").unwrap_or("");
                let err = exception_error(code, message);
                warn!("Bedrock stream error: {err}");
                Err(err)
            }
            other => Err(protocol_error(format!("unknown `:message-type` `{other}`"))),
        }
    }

    fn handle_event(
        &mut self,
        event: &str,
        payload: &[u8],
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        match event {
            "messageStart" => {
                let e: MessageStartEvent = parse_payload(event, payload)?;
                debug!("Bedrock messageStart role={}", e.role);
            }
            "contentBlockStart" => {
                let e: ContentBlockStartEvent = parse_payload(event, payload)?;
                self.block_start(e, tx)?;
            }
            "contentBlockDelta" => {
                let e: ContentBlockDeltaEvent = parse_payload(event, payload)?;
                self.block_delta(e, tx)?;
            }
            "contentBlockStop" => {
                let e: ContentBlockStopEvent = parse_payload(event, payload)?;
                self.block_stop(e.content_block_index, tx);
            }
            "messageStop" => {
                let e: MessageStopEvent = parse_payload(event, payload)?;
                self.stop = Some(map_stop_reason(&e.stop_reason));
            }
            "metadata" => {
                let e: MetadataEvent = parse_payload(event, payload)?;
                match e.usage {
                    Some(u) => self.usage = Some(u.into_usage()),
                    None => warn!("Bedrock metadata event without usage"),
                }
            }
            // An event type added after this was written may carry content
            // that is now being dropped — say so, once per type.
            other => {
                let other = other.to_string();
                self.warn_once(format!("event:{other}"), || {
                    format!("ignoring unknown ConverseStream event `{other}`")
                });
            }
        }
        Ok(())
    }

    fn block_start(
        &mut self,
        e: ContentBlockStartEvent,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let index = e.content_block_index;
        if self.blocks.contains_key(&index) {
            return Err(protocol_error(format!(
                "contentBlockStart for block {index}, which already started"
            )));
        }
        let start = e.start;
        let block = match start.tool_use {
            // A server-side tool runs on AWS's side and its result streams
            // back; it is not a call for the agent loop to execute.
            Some(t) if t.kind.as_deref() == Some("server_tool_use") => {
                warn!(
                    "Bedrock: server-side tool `{}` (block {index}) is not surfaced",
                    t.name
                );
                Block::new(BlockKind::Ignored, 0, t.name)
            }
            Some(t) => {
                let content_index = self.content.len();
                self.content.push(Content::ToolCall {
                    provider_metadata: None,
                    id: t.tool_use_id.clone(),
                    name: t.name.clone(),
                    arguments: unfinalized_arguments(""),
                });
                let _ = tx.send(StreamEvent::ToolCallStart {
                    content_index,
                    id: t.tool_use_id,
                    name: t.name.clone(),
                });
                Block::new(BlockKind::Tool, content_index, t.name)
            }
            None if start.image.is_some() || start.tool_result.is_some() => {
                let what = if start.image.is_some() {
                    "image"
                } else {
                    "toolResult"
                };
                warn!("Bedrock: {what} block {index} is not surfaced");
                Block::new(BlockKind::Ignored, 0, String::new())
            }
            // A start member this provider does not know: keep no block, so
            // the first delta types it rather than its content being dropped.
            None => {
                warn!(
                    "Bedrock: contentBlockStart for block {index} has no known member ({}); \
                     the block will be typed by its first delta",
                    member_names(&start.other)
                );
                return Ok(());
            }
        };
        self.blocks.insert(index, block);
        Ok(())
    }

    /// The block for `index`, created as `kind` if this is its first event
    /// (text and reasoning blocks have no `contentBlockStart`). `None` for a
    /// block this provider does not surface.
    fn block_for(
        &mut self,
        index: u64,
        kind: BlockKind,
    ) -> Result<Option<&mut Block>, ProviderError> {
        if !self.blocks.contains_key(&index) {
            let content_index = self.content.len();
            match kind {
                BlockKind::Text => self.content.push(Content::Text {
                    text: String::new(),
                }),
                BlockKind::Thinking => self.content.push(Content::thinking(String::new())),
                // A tool delta needs the id and name its start carried.
                BlockKind::Tool => {
                    return Err(protocol_error(format!(
                        "toolUse delta for block {index} without a contentBlockStart"
                    )))
                }
                BlockKind::Ignored => {}
            }
            self.blocks
                .insert(index, Block::new(kind, content_index, String::new()));
        }
        let block = self.blocks.get_mut(&index).expect("inserted above");
        if block.kind == BlockKind::Ignored {
            return Ok(None);
        }
        if block.kind != kind {
            return Err(protocol_error(format!(
                "block {index} is a {:?} block but received a {kind:?} delta",
                block.kind
            )));
        }
        if block.closed {
            return Err(protocol_error(format!(
                "delta for block {index} after its contentBlockStop"
            )));
        }
        Ok(Some(block))
    }

    fn block_delta(
        &mut self,
        e: ContentBlockDeltaEvent,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let index = e.content_block_index;
        let delta = e.delta;
        let known =
            delta.text.is_some() || delta.tool_use.is_some() || delta.reasoning_content.is_some();
        if let Some(text) = delta.text {
            if let Some(block) = self.block_for(index, BlockKind::Text)? {
                let ci = block.content_index;
                if let Some(Content::Text { text: t }) = self.content.get_mut(ci) {
                    t.push_str(&text);
                }
                let _ = tx.send(StreamEvent::TextDelta {
                    content_index: ci,
                    delta: text,
                });
            }
        }
        if let Some(tool) = delta.tool_use {
            if let Some(block) = self.block_for(index, BlockKind::Tool)? {
                block.input.push_str(&tool.input);
                let _ = tx.send(StreamEvent::ToolCallDelta {
                    content_index: block.content_index,
                    delta: tool.input,
                });
            }
        }
        if let Some(reasoning) = delta.reasoning_content {
            self.reasoning_delta(index, reasoning, tx)?;
        }
        // Union members this provider does not surface (`citation`,
        // `image`, `toolResult`, anything added later) and an empty delta.
        if !delta.other.is_empty() {
            let names = member_names(&delta.other);
            self.warn_once(format!("delta:{index}:{names}"), || {
                format!("dropping `{names}` content in block {index} (not surfaced)")
            });
        } else if !known {
            self.warn_once(format!("delta:{index}:empty"), || {
                format!("contentBlockDelta for block {index} has no member")
            });
        }
        Ok(())
    }

    fn reasoning_delta(
        &mut self,
        index: u64,
        reasoning: ReasoningContentBlockDelta,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let Some(block) = self.block_for(index, BlockKind::Thinking)? else {
            return Ok(());
        };
        let ci = block.content_index;
        if let Some(data) = &reasoning.redacted_content {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| {
                    protocol_error(format!(
                        "redactedContent in block {index} is not base64 ({e})"
                    ))
                })?;
            block.redacted.extend_from_slice(&bytes);
        }
        // Re-encoded only when this delta carried redacted data.
        let redacted = reasoning.redacted_content.is_some().then(|| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&block.redacted)
        });
        if let Some(Content::Thinking {
            thinking,
            signature,
            redacted: slot,
            redacted_protocol,
        }) = self.content.get_mut(ci)
        {
            if let Some(text) = reasoning.text {
                thinking.push_str(&text);
                let _ = tx.send(StreamEvent::ThinkingDelta {
                    content_index: ci,
                    delta: text,
                });
            }
            // A signature is a delta like any other member of this union:
            // pieces are appended. The API reference does not say it arrives
            // in one piece, and appending is the only reading under which a
            // split signature survives; a single delta is the same either way.
            if let Some(sig) = reasoning.signature {
                signature.get_or_insert_with(String::new).push_str(&sig);
            }
            if redacted.is_some() {
                *slot = redacted;
                *redacted_protocol = Some(crate::provider::ApiProtocol::BedrockConverseStream);
            }
        }
        Ok(())
    }

    fn block_stop(&mut self, index: u64, tx: &mpsc::UnboundedSender<StreamEvent>) {
        let Some(block) = self.blocks.get_mut(&index) else {
            debug!("Bedrock: contentBlockStop for unseen block {index}");
            return;
        };
        if block.closed {
            debug!("Bedrock: duplicate contentBlockStop for block {index}");
            return;
        }
        block.closed = true;
        if block.kind == BlockKind::Tool {
            let args = finalize_tool_arguments(&block.name, &block.input);
            if let Some(Content::ToolCall { arguments, .. }) =
                self.content.get_mut(block.content_index)
            {
                *arguments = args;
            }
            let _ = tx.send(StreamEvent::ToolCallEnd {
                content_index: block.content_index,
            });
        }
    }

    fn finish(
        self,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<StreamOutcome, ProviderError> {
        let Some((mut stop_reason, error_message)) = self.stop else {
            // No messageStop: the connection closed early, or the body held no
            // events at all. Either way this is not a finished response, and
            // must never read as an empty successful turn.
            let msg = format!(
                "Bedrock stream ended without a messageStop event ({} events received)",
                self.events
            );
            warn!("{msg}");
            return Err(ProviderError::Network(msg));
        };
        let mut content = self.content;
        let mut has_tool_calls = false;
        for block in self.blocks.values() {
            if block.kind != BlockKind::Tool {
                continue;
            }
            has_tool_calls = true;
            if !block.closed {
                // No contentBlockStop: the input may be cut short, and an
                // empty buffer cannot be told apart from a tool with no
                // parameters. Keep the marker so the call is answered with an
                // error, never run.
                warn!(
                    tool = %block.name,
                    "Bedrock tool call block was never closed; the call will not be run"
                );
                if let Some(Content::ToolCall { arguments, .. }) =
                    content.get_mut(block.content_index)
                {
                    *arguments = unfinalized_arguments(&block.input);
                }
                let _ = tx.send(StreamEvent::ToolCallEnd {
                    content_index: block.content_index,
                });
            }
        }
        if stop_reason == StopReason::ToolUse {
            let runnable = content.iter().any(|c| {
                matches!(c, Content::ToolCall { arguments, .. }
                    if crate::provider::unparsed_tool_arguments(arguments).is_none())
            });
            if !runnable {
                warn!("Bedrock: stopReason is tool_use but the response has no runnable tool call");
            }
        }
        // Same rule as the OpenAI-shaped providers: tool calls make this a
        // ToolUse turn, unless it hit the token limit (how a call ends up with
        // unparsed arguments), was refused, or failed.
        if has_tool_calls
            && !matches!(
                stop_reason,
                StopReason::Length | StopReason::Refusal | StopReason::Error
            )
        {
            stop_reason = StopReason::ToolUse;
        }
        // A stream that ends without usage reports zero tokens, as the other
        // providers do when their usage chunk never arrives: `Usage` has no
        // "unknown" state. The warning carries a structured `usage_missing`
        // field and is emitted inside the loop's `llm_stream` span, so
        // tracing/OTel consumers can find these turns.
        let usage = self.usage.unwrap_or_else(|| {
            warn!(
                usage_missing = true,
                "Bedrock stream carried no metadata usage; reporting zero tokens"
            );
            Usage::default()
        });
        Ok(StreamOutcome {
            content,
            usage,
            stop_reason,
            error_message,
        })
    }
}

/// Map a `MessageStopEvent.stopReason` onto [`StopReason`], with a diagnosis
/// for the terminal ones. Every documented value is listed ("Valid Values:
/// end_turn | tool_use | max_tokens | stop_sequence | guardrail_intervened |
/// content_filtered | malformed_model_output | malformed_tool_use |
/// model_context_window_exceeded").
fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "end_turn" | "stop_sequence" => (StopReason::Stop, None),
        "tool_use" => (StopReason::ToolUse, None),
        "max_tokens" => (StopReason::Length, None),
        "guardrail_intervened" => {
            warn!("Bedrock: a guardrail intervened (stopReason=guardrail_intervened)");
            (
                StopReason::Refusal,
                Some("Response blocked by an Amazon Bedrock guardrail (stopReason: guardrail_intervened)".into()),
            )
        }
        "content_filtered" => {
            warn!("Bedrock: response stopped by the content filter (stopReason=content_filtered)");
            (
                StopReason::Refusal,
                Some(
                    "Response stopped by the content filter (stopReason: content_filtered)".into(),
                ),
            )
        }
        // Same Error + overflow-phrase shape as the Anthropic provider, so
        // `Message::is_context_overflow()` and compaction keep working.
        "model_context_window_exceeded" => {
            warn!("Bedrock: context window exceeded mid-stream");
            (
                StopReason::Error,
                Some("model_context_window_exceeded".into()),
            )
        }
        "malformed_model_output" | "malformed_tool_use" => {
            warn!("Bedrock: the model produced malformed output (stopReason={reason})");
            (
                StopReason::Error,
                Some(format!(
                    "Bedrock stopped the response: the model produced malformed output (stopReason: {reason})"
                )),
            )
        }
        other => {
            warn!("unrecognized Bedrock stopReason '{other}'; treating it as a normal stop");
            (StopReason::Stop, None)
        }
    }
}

/// Budget for Bedrock's Anthropic-style thinking per level — the legacy
/// Anthropic budget table, shared so the two cannot drift. Unlike the
/// first-party path, Bedrock does not raise `maxTokens` above the budget.
fn bedrock_thinking_budget(level: ThinkingLevel) -> u32 {
    super::anthropic::legacy_thinking_budget(level)
}

fn build_bedrock_body(config: &StreamConfig) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    // Claude verifies replayed reasoning against its signature, so unsigned
    // reasoning (from another provider, after a model switch) cannot go back
    // to it. Bedrock's other reasoning models do not sign at all, and their
    // reasoning is replayed without one.
    let signed_reasoning_only = config.model.to_ascii_lowercase().contains("claude");

    for msg in &config.messages {
        match msg {
            Message::User { content, .. } => {
                let blocks = content_to_bedrock(content, signed_reasoning_only);
                messages.push(serde_json::json!({"role": "user", "content": blocks}));
            }
            Message::Assistant { content, .. } => {
                let blocks = content_to_bedrock(content, signed_reasoning_only);
                messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
            }
            Message::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                // Build content blocks for tool result (text + images)
                let tool_content: Vec<serde_json::Value> = content
                    .iter()
                    .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(serde_json::json!({"text": text})),
                        Content::Image { data, mime_type } => Some(serde_json::json!({
                            "image": {
                                "format": mime_type.split('/').nth(1).unwrap_or("png"),
                                "source": {"bytes": data},
                            }
                        })),
                        _ => None,
                    })
                    .collect();

                let tool_content = if tool_content.is_empty() {
                    vec![serde_json::json!({"text": ""})]
                } else {
                    tool_content
                };

                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "toolResult": {
                            "toolUseId": tool_call_id,
                            "content": tool_content,
                            "status": if *is_error { "error" } else { "success" },
                        }
                    }],
                }));
            }
        }
    }

    let mut body = serde_json::json!({"messages": messages});

    if !config.system_prompt.is_empty() {
        body["system"] = serde_json::json!([{"text": config.system_prompt}]);
    }

    let mut inference_config = serde_json::json!({});
    if let Some(max) = config.max_tokens {
        inference_config["maxTokens"] = serde_json::json!(max);
    }
    if let Some(temp) = config.temperature {
        inference_config["temperature"] = serde_json::json!(temp);
    }
    if inference_config != serde_json::json!({}) {
        body["inferenceConfig"] = inference_config;
    }

    // Thinking: Claude models on Bedrock take Anthropic's budget-based
    // thinking via additionalModelRequestFields (same budgets as the
    // pre-adaptive Anthropic path).
    if config.thinking_level != ThinkingLevel::Off {
        body["additionalModelRequestFields"] = serde_json::json!({
            "thinking": {
                "type": "enabled",
                "budget_tokens": bedrock_thinking_budget(config.thinking_level),
            }
        });
    }

    if !config.tools.is_empty() {
        let tools: Vec<serde_json::Value> = config
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "toolSpec": {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": {"json": t.parameters},
                    }
                })
            })
            .collect();
        body["toolConfig"] = serde_json::json!({"tools": tools});
    }

    body
}

fn content_to_bedrock(content: &[Content], signed_reasoning_only: bool) -> Vec<serde_json::Value> {
    content
        .iter()
        .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"text": text})),
            Content::Image { data, mime_type } => Some(serde_json::json!({
                "image": {
                    "format": mime_type.split('/').nth(1).unwrap_or("png"),
                    "source": {"bytes": data},
                }
            })),
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some(serde_json::json!({
                "toolUse": {"toolUseId": id, "name": name, "input": arguments},
            })),
            // Replay reasoning blocks: Anthropic-on-Bedrock requires the
            // thinking block to accompany a replayed assistant message in
            // multi-turn tool use, unmodified ("include the text and its
            // signature unmodified" — ReasoningTextBlock). The block is a
            // union (ReasoningContentBlock: `reasoningText` | `redactedContent`).
            // Only Bedrock's own `redactedContent` goes back: AWS does not
            // document it as the same bytes as Anthropic's `redacted_thinking`
            // data, so another API's encrypted reasoning is skipped.
            Content::Thinking {
                redacted: Some(_), ..
            } => match c.redacted_for(crate::provider::ApiProtocol::BedrockConverseStream) {
                Some(data) => Some(serde_json::json!({
                    "reasoningContent": {"redactedContent": data}
                })),
                None => {
                    debug!("Bedrock: skipping a redacted thinking block from another provider");
                    None
                }
            },
            Content::Thinking {
                thinking,
                signature: Some(signature),
                ..
            } if !signature.is_empty() => Some(serde_json::json!({
                "reasoningContent": {
                    "reasoningText": {"text": thinking, "signature": signature}
                }
            })),
            // Unsigned reasoning. `signature` is optional in
            // ReasoningTextBlock ("Required: No"), and Bedrock's non-Claude
            // reasoning models never sign, so it is sent without one — never
            // as `signature: ""`. Claude would reject it, so for a Claude
            // model it is skipped; that only happens after a switch from
            // another provider, so it logs at debug, not once per turn.
            Content::Thinking { thinking, .. } if !thinking.is_empty() => {
                if signed_reasoning_only {
                    debug!("Bedrock: not replaying unsigned reasoning to a Claude model");
                    None
                } else {
                    Some(serde_json::json!({
                        "reasoningContent": {"reasoningText": {"text": thinking}}
                    }))
                }
            }
            // Nothing to replay: no text, no signature, no redacted data.
            Content::Thinking { .. } => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ConverseStream event payloads.
//
// Each struct is the JSON payload of the frame whose `:event-type` header
// names it, as documented in the Amazon Bedrock Runtime API reference
// (docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_<Type>.html).
// Unknown fields are ignored: Bedrock pads payloads with an extra `p` field,
// and new optional members appear over time. Inside the two content unions
// (`ContentBlockStart`, `ContentBlockDelta`) unknown members are collected
// instead, so content this provider does not surface (`image`, `toolResult`,
// `citation`, anything added later) is dropped with a warning, not silently.
// ---------------------------------------------------------------------------

/// `MessageStartEvent`: "role — The role for the message. Valid Values: user
/// | assistant | system. Required: Yes".
/// Only logged, so a proxy that omits it does not fail the turn.
#[derive(Deserialize)]
struct MessageStartEvent {
    #[serde(default)]
    role: String,
}

/// `ContentBlockStartEvent`: "contentBlockIndex — The index for a content
/// block start event. Required: Yes"; "start — Start information about a
/// content block start event. Type: ContentBlockStart object ... a Union".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStartEvent {
    content_block_index: u64,
    start: ContentBlockStart,
}

/// `ContentBlockStart` (union): `toolUse` | `image` | `toolResult`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStart {
    #[serde(default)]
    tool_use: Option<ToolUseBlockStart>,
    #[serde(default)]
    image: Option<serde_json::Value>,
    #[serde(default)]
    tool_result: Option<serde_json::Value>,
    /// Any other member, named in a warning.
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

/// `ToolUseBlockStart`: `name` and `toolUseId` required; optional `type`
/// ("Valid Values: server_tool_use").
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolUseBlockStart {
    tool_use_id: String,
    name: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

/// `ContentBlockDeltaEvent`: "contentBlockIndex — The block index for a
/// content block delta event. Required: Yes"; "delta — The delta for a
/// content block delta event. Type: ContentBlockDelta object ... a Union".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockDeltaEvent {
    content_block_index: u64,
    delta: ContentBlockDelta,
}

/// `ContentBlockDelta` (union): `text` (String) | `toolUse`
/// (`ToolUseBlockDelta`) | `reasoningContent` (`ReasoningContentBlockDelta`)
/// | `citation` | `image` | `toolResult`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_use: Option<ToolUseBlockDelta>,
    #[serde(default)]
    reasoning_content: Option<ReasoningContentBlockDelta>,
    /// `citation`, `image`, `toolResult` or a member added later: not
    /// surfaced, named in a warning.
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

/// `ToolUseBlockDelta`: "input — The input for a requested tool. Type:
/// String. Required: Yes" — a fragment of the argument JSON text.
#[derive(Deserialize)]
struct ToolUseBlockDelta {
    input: String,
}

/// `ReasoningContentBlockDelta` (union): `text` | `signature` ("If you pass a
/// reasoning block back to the API in a multi-turn conversation, include the
/// text and its signature unmodified") | `redactedContent` (base64).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningContentBlockDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    redacted_content: Option<String>,
}

/// `ContentBlockStopEvent`: "contentBlockIndex — The index for a content
/// block. Required: Yes".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStopEvent {
    content_block_index: u64,
}

/// `MessageStopEvent`: "stopReason — The reason why the model stopped
/// generating output. Type: String ... Required: Yes";
/// `additionalModelResponseFields` (JSON, optional) is not used.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageStopEvent {
    stop_reason: String,
}

/// `ConverseStreamMetadataEvent`: `usage` (`TokenUsage`) and `metrics`
/// (`ConverseStreamMetrics`, `latencyMs`) are documented as required; only
/// usage is read, and its absence is tolerated (with a warning) rather than
/// failing an otherwise finished response.
#[derive(Deserialize)]
struct MetadataEvent {
    #[serde(default)]
    usage: Option<TokenUsage>,
}

/// `TokenUsage`: `inputTokens`, `outputTokens`, `totalTokens` required;
/// `cacheReadInputTokens` ("The number of input tokens read from the cache
/// for the request") and `cacheWriteInputTokens` ("... written to the cache
/// ...") optional.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    /// Copied through when present; a proxy that omits it gets 0 rather
    /// than a failed turn.
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_write_input_tokens: Option<u64>,
}

impl TokenUsage {
    fn into_usage(self) -> Usage {
        Usage {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            cache_write: self.cache_write_input_tokens.unwrap_or(0),
            total_tokens: self.total_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_blocks_are_replayed_with_signature() {
        // Anthropic-on-Bedrock rejects replayed assistant messages whose
        // thinking block was dropped — pin that we serialize it back.
        let mut config = StreamConfig::new("anthropic.claude-sonnet", "a:b");
        config.messages = vec![
            Message::user("go"),
            Message::assistant(
                vec![
                    Content::thinking_signed("chain of thought", "sig-1"),
                    Content::Text {
                        text: "answer".into(),
                    },
                ],
                StopReason::Stop,
                "m",
                "bedrock",
                Usage::default(),
            ),
        ];
        let body = build_bedrock_body(&config);
        let assistant_content = body["messages"][1]["content"].as_array().unwrap();
        let reasoning = assistant_content
            .iter()
            .find(|b| b.get("reasoningContent").is_some())
            .expect("thinking block must be replayed");
        assert_eq!(
            reasoning["reasoningContent"]["reasoningText"]["text"],
            "chain of thought"
        );
        assert_eq!(
            reasoning["reasoningContent"]["reasoningText"]["signature"],
            "sig-1"
        );
    }

    #[test]
    fn thinking_level_sets_additional_model_request_fields() {
        let config = StreamConfig {
            model: "anthropic.claude-sonnet".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: ThinkingLevel::High,
            api_key: "a:b".into(),
            max_tokens: Some(1024),
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };
        let body = build_bedrock_body(&config);
        let thinking = &body["additionalModelRequestFields"]["thinking"];
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(thinking["budget_tokens"], 8192);
    }

    #[test]
    fn xhigh_and_max_use_the_legacy_anthropic_budgets() {
        for (level, budget) in [(ThinkingLevel::XHigh, 16_384), (ThinkingLevel::Max, 30_720)] {
            let config = StreamConfig {
                model: "anthropic.claude-sonnet".into(),
                system_prompt: "".into(),
                messages: vec![Message::user("hi")],
                tools: vec![],
                thinking_level: level,
                api_key: "a:b".into(),
                max_tokens: Some(64_000),
                temperature: None,
                model_config: None,
                cache_config: CacheConfig::default(),
                output_schema: None,
            };
            let body = build_bedrock_body(&config);
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
                budget
            );
        }
    }

    #[test]
    fn thinking_off_omits_additional_fields() {
        let config = StreamConfig {
            model: "anthropic.claude-sonnet".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "a:b".into(),
            max_tokens: Some(1024),
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };
        let body = build_bedrock_body(&config);
        assert!(body["additionalModelRequestFields"].is_null());
    }

    #[test]
    fn test_build_bedrock_body() {
        let config = StreamConfig {
            model: "anthropic.claude-3-sonnet-20240229-v1:0".into(),
            system_prompt: "Be helpful".into(),
            messages: vec![Message::user("Hello")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "key:secret".into(),
            max_tokens: Some(1024),
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_bedrock_body(&config);
        assert!(body["messages"].is_array());
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["system"].is_array());
        assert_eq!(body["inferenceConfig"]["maxTokens"], 1024);
    }

    #[test]
    fn test_content_to_bedrock_filters_empty_text() {
        let content = vec![
            Content::Text { text: "".into() },
            Content::Text {
                text: "hello".into(),
            },
            Content::Text { text: "".into() },
        ];
        let blocks = content_to_bedrock(&content, false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], "hello");
    }

    #[test]
    fn test_content_to_bedrock() {
        let content = vec![
            Content::Text {
                text: "hello".into(),
            },
            Content::ToolCall {
                provider_metadata: None,
                id: "tc-1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
        ];
        let blocks = content_to_bedrock(&content, false);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "hello");
        assert_eq!(blocks[1]["toolUse"]["name"], "bash");
    }

    // -- Response stream ---------------------------------------------------

    use super::super::eventstream::{encode_frame, HeaderValue};

    fn event(event_type: &str, payload: serde_json::Value) -> Vec<u8> {
        encode_frame(
            &[
                (":message-type", HeaderValue::String("event".into())),
                (":event-type", HeaderValue::String(event_type.into())),
                (
                    ":content-type",
                    HeaderValue::String("application/json".into()),
                ),
            ],
            payload.to_string().as_bytes(),
        )
    }

    /// A text + tool-call response whose text has multi-byte characters.
    fn full_response() -> Vec<u8> {
        use serde_json::json;
        [
            event("messageStart", json!({"role": "assistant"})),
            event(
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": "héllo 世界 🌍"}}),
            ),
            event("contentBlockStop", json!({"contentBlockIndex": 0})),
            event(
                "contentBlockStart",
                json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "t1", "name": "read"}}}),
            ),
            event(
                "contentBlockDelta",
                json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"path\":"}}}),
            ),
            event(
                "contentBlockDelta",
                json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "\"ß.txt\"}"}}}),
            ),
            event("contentBlockStop", json!({"contentBlockIndex": 1})),
            event("messageStop", json!({"stopReason": "tool_use"})),
            event(
                "metadata",
                json!({"usage": {"inputTokens": 5, "outputTokens": 7, "totalTokens": 12}, "metrics": {"latencyMs": 3}}),
            ),
        ]
        .concat()
    }

    async fn read_chunks(
        chunks: Vec<Result<Vec<u8>, String>>,
    ) -> Result<StreamOutcome, ProviderError> {
        let (tx, _rx) = mpsc::unbounded_channel();
        read_converse_stream(
            futures::stream::iter(chunks),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
    }

    /// The provider's byte-stream path buffers across network chunks: every
    /// chunk size from 1 byte up splits frames inside the prelude, inside
    /// headers and inside multi-byte UTF-8 characters, and the result is the
    /// same as one chunk.
    #[tokio::test]
    async fn byte_stream_is_buffered_across_arbitrary_chunks() {
        let body = full_response();
        for size in 1..=body.len() {
            let chunks = body.chunks(size).map(|c| Ok(c.to_vec())).collect();
            let out = read_chunks(chunks)
                .await
                .unwrap_or_else(|e| panic!("chunk size {size}: {e}"));
            assert_eq!(out.stop_reason, StopReason::ToolUse, "chunk size {size}");
            assert!(
                matches!(&out.content[0], Content::Text { text } if text == "héllo 世界 🌍"),
                "chunk size {size}: {:?}",
                out.content
            );
            assert!(
                matches!(&out.content[1], Content::ToolCall { arguments, .. }
                    if *arguments == serde_json::json!({"path": "ß.txt"})),
                "chunk size {size}: {:?}",
                out.content
            );
            assert_eq!(out.usage.total_tokens, 12);
        }
    }

    #[tokio::test]
    async fn transport_error_mid_stream_is_a_network_error() {
        let body = full_response();
        let (head, _) = body.split_at(body.len() / 2);
        let result = read_chunks(vec![Ok(head.to_vec()), Err("connection reset".into())]).await;
        assert!(
            matches!(result, Err(ProviderError::Network(_))),
            "{:?}",
            result.err()
        );
    }

    /// Cancellation wins over a stream that has stalled mid-response.
    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_stream() {
        use futures::StreamExt as _;
        let first: Vec<Result<Vec<u8>, String>> = vec![Ok(event(
            "messageStart",
            serde_json::json!({"role": "assistant"}),
        ))];
        let stream = futures::stream::iter(first).chain(futures::stream::pending());
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_converse_stream(stream, &tx, &cancel),
        )
        .await
        .expect("cancellation must end the read");
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }

    #[test]
    fn reasoning_replay_rules() {
        let content = [
            Content::thinking("unsigned reasoning"),
            Content::thinking(""),
            Content::thinking_redacted(crate::provider::ApiProtocol::BedrockConverseStream, "AAEC"),
            Content::thinking_signed("real", "sig"),
            Content::Text { text: "t".into() },
        ];
        let redacted = serde_json::json!({"reasoningContent": {"redactedContent": "AAEC"}});
        let signed = serde_json::json!(
            {"reasoningContent": {"reasoningText": {"text": "real", "signature": "sig"}}}
        );
        let text = serde_json::json!({"text": "t"});

        // A model that does not sign gets unsigned reasoning back, with no
        // signature key at all (never `""`); an empty block is dropped.
        assert_eq!(
            content_to_bedrock(&content, false),
            vec![
                serde_json::json!(
                    {"reasoningContent": {"reasoningText": {"text": "unsigned reasoning"}}}
                ),
                redacted.clone(),
                signed.clone(),
                text.clone(),
            ]
        );
        // Claude verifies signatures: unsigned reasoning is not replayed.
        assert_eq!(
            content_to_bedrock(&content, true),
            vec![redacted, signed, text]
        );
    }

    #[test]
    fn claude_model_ids_replay_signed_reasoning_only() {
        let unsigned = || Message::Assistant {
            content: vec![
                Content::thinking("mine"),
                Content::Text { text: "a".into() },
            ],
            stop_reason: StopReason::Stop,
            model: String::new(),
            provider: String::new(),
            usage: Usage::default(),
            timestamp: 0,
            error_message: None,
        };
        let body_for = |model: &str| {
            build_bedrock_body(&StreamConfig {
                model: model.into(),
                system_prompt: String::new(),
                messages: vec![unsigned()],
                tools: vec![],
                thinking_level: ThinkingLevel::Off,
                api_key: "key:secret".into(),
                max_tokens: Some(1024),
                temperature: None,
                model_config: None,
                cache_config: CacheConfig::default(),
                output_schema: None,
            })
        };
        let reasoning_blocks = |body: serde_json::Value| {
            body["messages"][0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b.get("reasoningContent").is_some())
                .count()
        };
        assert_eq!(
            reasoning_blocks(body_for("us.anthropic.claude-sonnet-5-v1:0")),
            0
        );
        assert_eq!(reasoning_blocks(body_for("openai.gpt-oss-120b-1:0")), 1);
    }

    #[tokio::test]
    async fn zero_bytes_is_an_error_not_an_empty_turn() {
        let result = read_chunks(vec![]).await;
        assert!(matches!(result, Err(ProviderError::Network(_))));
    }

    #[test]
    fn exceptions_classify_like_their_http_status() {
        assert!(matches!(
            exception_error("throttlingException", "Too many requests"),
            ProviderError::RateLimited { .. }
        ));
        assert!(exception_error(
            "validationException",
            "Input is too long for requested model."
        )
        .is_context_overflow());
        assert!(matches!(
            exception_error("validationException", "bad field"),
            ProviderError::Api(_)
        ));
        assert!(matches!(
            exception_error("modelStreamErrorException", "boom"),
            ProviderError::Api(_)
        ));
    }

    #[test]
    fn every_documented_stop_reason_is_mapped() {
        use StopReason::*;
        for (reason, expected) in [
            ("end_turn", Stop),
            ("stop_sequence", Stop),
            ("tool_use", ToolUse),
            ("max_tokens", Length),
            ("guardrail_intervened", Refusal),
            ("content_filtered", Refusal),
            ("malformed_model_output", Error),
            ("malformed_tool_use", Error),
            ("model_context_window_exceeded", Error),
        ] {
            assert_eq!(map_stop_reason(reason).0, expected, "{reason}");
        }
        let (_, msg) = map_stop_reason("model_context_window_exceeded");
        assert!(crate::provider::traits::is_context_overflow_message(
            &msg.unwrap()
        ));
    }

    // --- Authentication (#174) -------------------------------------------

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            vars.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    fn no_env(_: &str) -> Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    fn sigv4_of(auth: Auth) -> Credentials {
        match auth {
            Auth::SigV4(c) => c,
            other => panic!("expected SigV4, got {other:?}"),
        }
    }

    #[test]
    fn api_key_with_colons_is_iam_credentials() {
        let c = sigv4_of(parse_api_key("AKID:SECRET").unwrap());
        assert_eq!(c.access_key_id, "AKID");
        assert_eq!(c.secret_access_key, "SECRET");
        assert_eq!(c.session_token, None);
        let c = sigv4_of(parse_api_key("AKID:SECRET:TOKEN").unwrap());
        assert_eq!(c.session_token.as_deref(), Some("TOKEN"));
        // An empty third part is no token.
        let c = sigv4_of(parse_api_key("AKID:SECRET:").unwrap());
        assert_eq!(c.session_token, None);
        for bad in [":SECRET", "AKID:", ":", "AKID::TOKEN"] {
            let err = parse_api_key(bad).unwrap_err();
            assert!(matches!(err, ProviderError::Auth(_)), "{bad}");
        }
    }

    #[test]
    fn api_key_without_colon_is_a_bearer_token() {
        match parse_api_key("bedrock-api-key-YmVkcm9jay5hbWF6b25hd3MuY29t").unwrap() {
            Auth::Bearer(t) => assert_eq!(t, "bedrock-api-key-YmVkcm9jay5hbWF6b25hd3MuY29t"),
            other => panic!("{other:?}"),
        }
        // A lone access key id is refused rather than sent as a bearer token.
        assert!(matches!(
            parse_api_key("AKIAIOSFODNN7EXAMPLE"),
            Err(ProviderError::Auth(_))
        ));
        assert!(matches!(
            parse_api_key("ASIAIOSFODNN7EXAMPLE"),
            Err(ProviderError::Auth(_))
        ));
    }

    #[test]
    fn auth_precedence() {
        let mut headers = HashMap::new();
        let env = env_of(&[
            ("AWS_BEARER_TOKEN_BEDROCK", "env-token"),
            ("AWS_ACCESS_KEY_ID", "ENVAKID"),
            ("AWS_SECRET_ACCESS_KEY", "ENVSECRET"),
        ]);
        // An explicit api_key beats the environment.
        assert!(matches!(
            resolve_auth("AKID:SECRET", &headers, &env).unwrap(),
            Auth::SigV4(_)
        ));
        // Empty api_key: the bearer env var beats IAM env credentials.
        match resolve_auth("", &headers, &env).unwrap() {
            Auth::Bearer(t) => assert_eq!(t, "env-token"),
            other => panic!("{other:?}"),
        }
        // An authorization header (any case) beats everything.
        headers.insert("Authorization".to_string(), "Custom x".to_string());
        assert!(matches!(
            resolve_auth("AKID:SECRET", &headers, &env).unwrap(),
            Auth::Explicit
        ));
    }

    #[test]
    fn env_iam_credentials_with_session_token() {
        let env = env_of(&[
            ("AWS_BEARER_TOKEN_BEDROCK", "  "),
            ("AWS_ACCESS_KEY_ID", "ENVAKID"),
            ("AWS_SECRET_ACCESS_KEY", "ENVSECRET"),
            ("AWS_SESSION_TOKEN", "ENVTOKEN"),
        ]);
        let c = sigv4_of(resolve_auth("", &HashMap::new(), &env).unwrap());
        assert_eq!(c.access_key_id, "ENVAKID");
        assert_eq!(c.secret_access_key, "ENVSECRET");
        assert_eq!(c.session_token.as_deref(), Some("ENVTOKEN"));
    }

    #[test]
    fn no_credentials_is_an_auth_error() {
        let err = resolve_auth("", &HashMap::new(), &no_env).unwrap_err();
        assert!(
            matches!(err, ProviderError::Auth(ref m) if m.contains("AWS_BEARER_TOKEN_BEDROCK"))
        );
        // Half a credential pair is none.
        let env = env_of(&[("AWS_ACCESS_KEY_ID", "ENVAKID")]);
        assert!(resolve_auth("", &HashMap::new(), &env).is_err());
    }

    #[test]
    fn auth_debug_never_prints_secrets() {
        let s = format!(
            "{:?}",
            parse_api_key("AKID:SUPERSECRET:SESSIONTOK").unwrap()
        );
        assert!(s.contains("AKID"));
        assert!(
            !s.contains("SUPERSECRET") && !s.contains("SESSIONTOK"),
            "{s}"
        );
        let s = format!("{:?}", parse_api_key("bearer-secret-value").unwrap());
        assert!(!s.contains("bearer-secret-value"), "{s}");
    }

    #[test]
    fn region_comes_from_the_endpoint_host() {
        for (host, region) in [
            ("bedrock-runtime.us-east-1.amazonaws.com", Some("us-east-1")),
            (
                "bedrock-runtime.eu-central-1.amazonaws.com",
                Some("eu-central-1"),
            ),
            ("BEDROCK-RUNTIME.us-west-2.amazonaws.com", Some("us-west-2")),
            (
                "bedrock-runtime-fips.us-gov-west-1.amazonaws.com",
                Some("us-gov-west-1"),
            ),
            (
                "bedrock-runtime.cn-north-1.amazonaws.com.cn",
                Some("cn-north-1"),
            ),
            (
                "vpce-0abc-xyz.bedrock-runtime.ap-southeast-2.vpce.amazonaws.com",
                Some("ap-southeast-2"),
            ),
            ("127.0.0.1", None),
            ("localhost", None),
            ("bedrock-runtime.example.com", None),
            ("bedrock-runtime.amazonaws.com", None),
            // Dual-stack endpoints (partition `dualStackDnsSuffix`).
            ("bedrock-runtime.us-east-1.api.aws", Some("us-east-1")),
            ("bedrock-runtime-fips.us-west-2.api.aws", Some("us-west-2")),
            (
                "bedrock-runtime.cn-north-1.api.amazonwebservices.com.cn",
                Some("cn-north-1"),
            ),
            ("bedrock-runtime.us-east-1.api.aws.example.com", None),
            ("bedrock-runtime.us-east-1.amazonaws.com.evil.example", None),
            ("bedrock.us-east-1.amazonaws.com", None),
        ] {
            assert_eq!(region_from_host(host).as_deref(), region, "{host}");
        }
    }

    #[test]
    fn region_precedence_host_then_aws_region_then_default() {
        let both = env_of(&[
            ("AWS_REGION", "eu-west-1"),
            ("AWS_DEFAULT_REGION", "ap-south-1"),
        ]);
        // The host wins: signing for another region than the host's fails.
        assert_eq!(
            signing_region("bedrock-runtime.us-east-1.amazonaws.com", &both).unwrap(),
            "us-east-1"
        );
        assert_eq!(signing_region("127.0.0.1", &both).unwrap(), "eu-west-1");
        let default_only = env_of(&[("AWS_DEFAULT_REGION", "ap-south-1")]);
        assert_eq!(
            signing_region("127.0.0.1", &default_only).unwrap(),
            "ap-south-1"
        );
        assert!(matches!(
            signing_region("127.0.0.1", &no_env),
            Err(ProviderError::Auth(_))
        ));
    }

    /// The model id is encoded once on the wire and again in the canonical
    /// URI, for both a `:` version suffix and an ARN's `/`.
    #[test]
    fn signed_path_double_encodes_the_model_id() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        for (model, wire, canonical) in [
            (
                "anthropic.claude-sonnet-5-v1:0",
                "/model/anthropic.claude-sonnet-5-v1%3A0/converse-stream",
                "/model/anthropic.claude-sonnet-5-v1%253A0/converse-stream",
            ),
            (
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.x",
                "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Fus.anthropic.x/converse-stream",
                "/model/arn%253Aaws%253Abedrock%253Aus-east-1%253A123456789012%253Ainference-profile%252Fus.anthropic.x/converse-stream",
            ),
        ] {
            let url = reqwest::Url::parse(&format!(
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/{}/converse-stream",
                sigv4::uri_encode(model)
            ))
            .unwrap();
            assert_eq!(url.path(), wire);
            let signed = sigv4::sign(
                "POST",
                url.path(),
                &[("host", "bedrock-runtime.us-east-1.amazonaws.com")],
                b"{}",
                &sigv4::SigningParams {
                    credentials: &creds,
                    region: "us-east-1",
                    service: SIGNING_NAME,
                    amz_date: "20260928T000000Z",
                    sign_content_sha256: true,
                },
            );
            assert_eq!(signed.canonical_request.lines().nth(1), Some(canonical));
        }
    }

    // --- Review round: guards (#174) ---------------------------------------

    const SECRET_40: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    fn auth_message(r: Result<Auth, ProviderError>) -> String {
        match r {
            Err(ProviderError::Auth(m)) => m,
            other => panic!("expected an Auth error, got {other:?}"),
        }
    }

    /// IAM credentials joined with anything but `:` (or a lone secret) must
    /// never become a bearer token, and the refusal never echoes the value.
    #[test]
    fn malformed_iam_keys_are_not_sent_as_bearer_tokens() {
        for bad in [
            format!("AKIAIOSFODNN7EXAMPLE {SECRET_40}"),
            format!("AKIAIOSFODNN7EXAMPLE\t{SECRET_40}"),
            format!("AKIAIOSFODNN7EXAMPLE;{SECRET_40}"),
            format!("AKIAIOSFODNN7EXAMPLE,{SECRET_40}"),
            format!("AKIAIOSFODNN7EXAMPLE/{SECRET_40}"),
            format!("ASIAIOSFODNN7EXAMPLE/{SECRET_40}"),
            SECRET_40.to_string(),
            "AKIAIOSFODNN7EXAMPLE".to_string(),
            "AKIA".to_string(),
            "bedrock-api-key-abc\ndef".to_string(),
            "bedrock-api-key-abc.def".to_string(),
            "ABSK@example".to_string(),
        ] {
            let m = auth_message(parse_api_key(&bad));
            assert!(m.starts_with("api_key is not a Bedrock API key"), "{m}");
            assert!(!m.contains(SECRET_40) && !m.contains("EXAMPLE"), "{m}");
        }
        // The reason names what is wrong: a separator between two keys, not
        // just "odd characters".
        let m = auth_message(parse_api_key(&format!("AKIAIOSFODNN7EXAMPLE {SECRET_40}")));
        assert!(m.contains("whitespace"), "{m}");
        let m = auth_message(parse_api_key("bedrock-api-key-abc\rdef"));
        assert!(m.contains("control characters"), "{m}");
        // Real key shapes pass: short-term (prefix + base64), long-term
        // (`ABSK` + base64), and an unknown base64 shape (warned, not refused).
        for good in [
            "bedrock-api-key-YmVkcm9jay5hbWF6b25hd3MuY29tLz9BY3Rpb249Q2FsbA==",
            "ABSKQmVkcm9ja0FQSUtleS1leGFtcGxlK3NlY3JldA==",
            "c29tZS1vdGhlci1iZWFyZXItdG9rZW4tc2hhcGU",
        ] {
            assert!(matches!(parse_api_key(good), Ok(Auth::Bearer(_))), "{good}");
        }
        // A 40-character value with a known prefix is a key, not a secret.
        let forty = format!("ABSK{}", "A".repeat(36));
        assert!(matches!(parse_api_key(&forty), Ok(Auth::Bearer(_))));
    }

    #[test]
    fn bearer_env_var_is_checked_too() {
        let env = env_of(&[(
            "AWS_BEARER_TOKEN_BEDROCK",
            "AKIAIOSFODNN7EXAMPLE zzhiddenzz",
        )]);
        let m = auth_message(resolve_auth("", &HashMap::new(), &env));
        assert!(m.starts_with("AWS_BEARER_TOKEN_BEDROCK is not"), "{m}");
        assert!(!m.contains("zzhiddenzz") && !m.contains("EXAMPLE"), "{m}");
    }

    #[test]
    fn temporary_credentials_need_a_session_token() {
        let m = auth_message(parse_api_key(&format!("ASIAIOSFODNN7EXAMPLE:{SECRET_40}")));
        assert!(m.contains(":session_token is required"), "{m}");
        assert!(!m.contains(SECRET_40));
        assert!(matches!(
            parse_api_key(&format!("ASIAIOSFODNN7EXAMPLE:{SECRET_40}:TOKEN")),
            Ok(Auth::SigV4(_))
        ));
        // Long-term (AKIA) keys need none.
        assert!(matches!(
            parse_api_key(&format!("AKIAIOSFODNN7EXAMPLE:{SECRET_40}")),
            Ok(Auth::SigV4(_))
        ));
        let env = env_of(&[
            ("AWS_ACCESS_KEY_ID", "ASIAIOSFODNN7EXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", SECRET_40),
        ]);
        let m = auth_message(resolve_auth("", &HashMap::new(), &env));
        assert!(m.contains("set AWS_SESSION_TOKEN"), "{m}");
    }

    #[test]
    fn half_an_env_pair_names_the_missing_half() {
        let env = env_of(&[
            ("AWS_ACCESS_KEY_ID", "AKID"),
            ("AWS_SECRET_ACCESS_KEY", " "),
        ]);
        let m = auth_message(resolve_auth("", &HashMap::new(), &env));
        assert!(
            m.contains("AWS_SECRET_ACCESS_KEY is missing or empty"),
            "{m}"
        );
        let env = env_of(&[("AWS_SECRET_ACCESS_KEY", SECRET_40)]);
        let m = auth_message(resolve_auth("", &HashMap::new(), &env));
        assert!(m.contains("AWS_ACCESS_KEY_ID is missing or empty"), "{m}");
        assert!(!m.contains(SECRET_40));
    }

    #[test]
    fn non_unicode_env_values_are_reported_not_ignored() {
        let env = |name: &str| match name {
            "AWS_ACCESS_KEY_ID" => Ok("AKID".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Err(std::env::VarError::NotUnicode(
                std::ffi::OsString::from("x"),
            )),
            _ => Err(std::env::VarError::NotPresent),
        };
        let m = auth_message(resolve_auth("", &HashMap::new(), &env));
        assert_eq!(m, "AWS_SECRET_ACCESS_KEY is set but is not valid Unicode");
    }

    fn url() -> reqwest::Url {
        reqwest::Url::parse(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/m%3A0/converse-stream",
        )
        .unwrap()
    }

    fn fixed_now() -> Result<String, String> {
        Ok("20260928T000000Z".into())
    }

    fn creds(access: &str, token: Option<&str>) -> Auth {
        Auth::SigV4(Credentials {
            access_key_id: access.into(),
            secret_access_key: SECRET_40.into(),
            session_token: token.map(str::to_string),
        })
    }

    fn headers_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A value reqwest cannot put in a header is an error naming the header
    /// (not a retried "builder error"), and never shows the value.
    #[test]
    fn invalid_header_values_are_errors_naming_the_header() {
        let none = HashMap::new();
        let cases = [
            (Auth::Bearer("bedrock-api-key-a\nb".into()), "authorization"),
            (creds("AKÍD", None), "authorization"),
            (creds("AKID", Some("zzT\nzz")), "x-amz-security-token"),
        ];
        for (auth, name) in cases {
            let err = request_headers(&url(), b"{}", auth, &none, &no_env, fixed_now).unwrap_err();
            match err {
                ProviderError::Auth(m) => {
                    assert!(m.contains(&format!("`{name}`")), "{m}");
                    assert!(!m.contains("zzT") && !m.contains("a\nb"), "{m}");
                }
                other => panic!("{other:?}"),
            }
        }
        let bad_user = headers_of(&[("x-custom", "a\nsecret")]);
        let err = request_headers(
            &url(),
            b"{}",
            Auth::Bearer("k".into()),
            &bad_user,
            &no_env,
            fixed_now,
        )
        .unwrap_err();
        assert!(
            matches!(&err, ProviderError::Other(m) if m.contains("`x-custom`") && !m.contains("secret")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_request_that_cannot_be_built_is_not_a_network_error() {
        let err = reqwest::Client::new()
            .post("not a url")
            .send()
            .await
            .unwrap_err();
        assert!(err.is_builder());
        assert!(matches!(send_error(err), ProviderError::Other(_)));
    }

    #[test]
    fn credential_headers_are_marked_sensitive() {
        let none = HashMap::new();
        let h = request_headers(
            &url(),
            b"{}",
            creds("AKID", Some("TOKEN")),
            &none,
            &no_env,
            fixed_now,
        )
        .unwrap();
        assert!(h["authorization"].is_sensitive());
        assert!(h["x-amz-security-token"].is_sensitive());
        assert!(!h["x-amz-date"].is_sensitive());
        assert!(!format!("{h:?}").contains("TOKEN"));
        let h = request_headers(
            &url(),
            b"{}",
            Auth::Bearer("bedrock-api-key-x".into()),
            &none,
            &no_env,
            fixed_now,
        )
        .unwrap();
        assert!(h["authorization"].is_sensitive());
        let explicit = headers_of(&[("Authorization", "precomputed")]);
        let h =
            request_headers(&url(), b"{}", Auth::Explicit, &explicit, &no_env, fixed_now).unwrap();
        assert!(h["authorization"].is_sensitive());
    }

    /// On the SigV4 path a user header with a signed name would be sent
    /// twice and break the signature: refused, naming the header.
    #[test]
    fn user_headers_colliding_with_signed_headers_are_refused() {
        for name in [
            "Content-Type",
            "host",
            "X-Amz-Date",
            "x-amz-security-token",
            "x-amz-content-sha256",
        ] {
            let extra = headers_of(&[(name, "v")]);
            let err = request_headers(
                &url(),
                b"{}",
                creds("AKID", None),
                &extra,
                &no_env,
                fixed_now,
            )
            .unwrap_err();
            assert!(
                matches!(&err, ProviderError::Auth(m) if m.contains(&format!("`{name}`"))),
                "{err:?}"
            );
        }
        // Unrelated headers are fine, and bearer auth keeps the old append
        // behaviour for every header.
        let extra = headers_of(&[("x-custom", "v")]);
        assert!(request_headers(
            &url(),
            b"{}",
            creds("AKID", None),
            &extra,
            &no_env,
            fixed_now
        )
        .is_ok());
        let extra = headers_of(&[("content-type", "application/json")]);
        assert!(request_headers(
            &url(),
            b"{}",
            Auth::Bearer("k".into()),
            &extra,
            &no_env,
            fixed_now
        )
        .is_ok());
    }

    #[test]
    fn a_clock_before_the_epoch_is_an_error_not_a_1970_signature() {
        fn broken() -> Result<String, String> {
            sigv4::amz_date_at(std::time::UNIX_EPOCH - std::time::Duration::from_secs(5))
        }
        let err = request_headers(
            &url(),
            b"{}",
            creds("AKID", None),
            &HashMap::new(),
            &no_env,
            broken,
        )
        .unwrap_err();
        assert!(
            matches!(&err, ProviderError::Auth(m) if m.contains("clock")),
            "{err:?}"
        );
    }
}
