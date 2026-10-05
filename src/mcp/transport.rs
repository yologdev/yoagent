//! MCP transport implementations: stdio and HTTP+SSE.

use super::types::*;
#[cfg(feature = "native")]
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::BTreeMap;
#[cfg(feature = "native")]
use std::collections::HashMap;
#[cfg(feature = "native")]
use std::sync::Arc;
#[cfg(feature = "native")]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(feature = "native")]
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Transport trait for MCP communication.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait McpTransport: crate::rt::MaybeSend + crate::rt::MaybeSync {
    /// Send a JSON-RPC request and receive the response.
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError>;
    /// Send a JSON-RPC notification, which gets no response.
    ///
    /// The default sends it through [`send`](Self::send) as a request and
    /// waits for an answer — what this crate did before notifications had
    /// their own path. A spec-compliant server never answers a notification,
    /// so a transport should override this (the built-in ones do) to write it
    /// and return.
    async fn notify(&self, notification: JsonRpcNotification) -> Result<(), McpError> {
        self.send(JsonRpcRequest::new(
            notification.method,
            notification.params,
        ))
        .await
        .map(|_| ())
    }
    /// Close the transport.
    async fn close(&self) -> Result<(), McpError>;
}

// ---------------------------------------------------------------------------
// Stdio Transport
// ---------------------------------------------------------------------------

/// Communicates with an MCP server via stdin/stdout of a child process.
/// One JSON-RPC message per line (newline-delimited JSON). Native hosts only.
///
/// Each request waits for the response carrying its own id; the server's own
/// messages in between are handled (notifications skipped, `ping` answered,
/// other server requests declined). The server's stderr is drained
/// continuously — the last few KB are kept, so an error from a server that
/// exited can say why — and the process is killed when the transport is
/// dropped.
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
pub struct StdioTransport {
    stdin: Arc<Mutex<StdinWriter>>,
    stdout: Arc<Mutex<BufReader<tokio::process::ChildStdout>>>,
    child: Arc<Mutex<Child>>,
    stderr_tail: Arc<std::sync::Mutex<StderrTail>>,
    /// The stderr drain; awaited briefly when the server exits, so its last
    /// words are in the tail before the error is built.
    stderr_drain: Mutex<Option<tokio::task::JoinHandle<()>>>,
    command: String,
}

/// The server's stdin, and whether a write was cut off mid-message (a call
/// cancelled or timed out while the server was not reading).
#[cfg(feature = "native")]
struct StdinWriter {
    stdin: tokio::process::ChildStdin,
    interrupted: bool,
}

/// The last lines the server wrote to stderr, at most [`StderrTail::MAX_BYTES`].
#[cfg(feature = "native")]
#[derive(Default)]
struct StderrTail {
    lines: std::collections::VecDeque<String>,
    bytes: usize,
}

#[cfg(feature = "native")]
impl StderrTail {
    const MAX_BYTES: usize = 4096;

    fn push(&mut self, line: String) {
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.bytes > Self::MAX_BYTES && self.lines.len() > 1 {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len();
            }
        }
    }

    fn render(&self) -> String {
        self.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

#[cfg(feature = "native")]
impl StdioTransport {
    /// Spawn a child process and create a stdio transport.
    pub async fn new(
        command: &str,
        args: &[&str],
        env: Option<HashMap<String, String>>,
    ) -> Result<Self, McpError> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Dropping the transport (or the client) ends the server.
            .kill_on_drop(true);

        if let Some(env_vars) = env {
            for (k, v) in env_vars {
                cmd.env(k, v);
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Transport(format!("Failed to spawn '{}': {}", command, e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("Failed to capture stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("Failed to capture stdout".into()))?;

        let stderr_tail = Arc::new(std::sync::Mutex::new(StderrTail::default()));
        let stderr_drain = child.stderr.take().map(|stderr| {
            tokio::spawn(drain_stderr(
                stderr,
                command.to_string(),
                stderr_tail.clone(),
            ))
        });

        Ok(Self {
            stdin: Arc::new(Mutex::new(StdinWriter {
                stdin,
                interrupted: false,
            })),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            child: Arc::new(Mutex::new(child)),
            stderr_tail,
            stderr_drain: Mutex::new(stderr_drain),
            command: command.to_string(),
        })
    }

    /// Write one newline-terminated message to the server.
    async fn write_message(&self, json: &str) -> Result<(), McpError> {
        let mut writer = self.stdin.lock().await;
        // A message cut off by a cancelled call is still on the line: start
        // this one on a fresh line, so the server sees one bad line (which it
        // answers with a parse error, or ignores) rather than two.
        let message = if writer.interrupted {
            format!("\n{json}\n")
        } else {
            format!("{json}\n")
        };
        writer.interrupted = true; // until the write completes
        writer
            .stdin
            .write_all(message.as_bytes())
            .await
            .map_err(|e| McpError::Transport(format!("Write error: {}", e)))?;
        writer
            .stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(format!("Flush error: {}", e)))?;
        writer.interrupted = false;
        Ok(())
    }

    /// The error for a server that closed its stdout: its exit status (when
    /// it has exited) and the last of its stderr, which is where servers say
    /// why they failed (a missing package, a missing API key, a traceback).
    async fn closed_error(&self, skipped: &str) -> McpError {
        // Stdout closing can be seen before the drain has read the last of
        // stderr; give it a moment to reach the end.
        // Taken, not borrowed: a finished JoinHandle must not be polled again
        // by a later call that also finds the connection closed.
        let drain = self.stderr_drain.lock().await.take();
        if let Some(drain) = drain {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), drain).await;
        }
        let status = match self.child.lock().await.try_wait() {
            Ok(Some(status)) => format!("exited with {status}"),
            _ => "closed its output".to_string(),
        };
        let tail = self
            .stderr_tail
            .lock()
            .map(|t| t.render())
            .unwrap_or_default();
        let tail = if tail.trim().is_empty() {
            String::new()
        } else {
            format!("; last stderr:\n{tail}")
        };
        McpError::Transport(format!(
            "Connection closed: MCP server '{}' {status}{skipped}{tail}",
            self.command
        ))
    }
}

/// Read the server's stderr to the end, logging each line and keeping the
/// last few KB. Bytes are decoded lossily and lines are read in bounded
/// chunks, so neither binary output nor a huge line stops the draining — a
/// piped stderr nobody reads fills up and blocks the server.
#[cfg(feature = "native")]
async fn drain_stderr(
    stderr: tokio::process::ChildStderr,
    server: String,
    tail: Arc<std::sync::Mutex<StderrTail>>,
) {
    use tokio::io::AsyncReadExt;
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut reader).take(8192).read_until(b'\n', &mut buf).await {
            Ok(0) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                if line.is_empty() {
                    continue;
                }
                debug!(target: "yoagent::mcp::stderr", server = %server, "{line}");
                if let Ok(mut tail) = tail.lock() {
                    tail.push(line);
                }
            }
            Err(e) => {
                warn!("MCP server '{server}': reading its stderr failed ({e}); it may block if it keeps writing");
                return;
            }
        }
    }
}

/// The reply to a request the *server* sent: `ping` succeeds; anything else
/// (sampling, roots, elicitation — capabilities this client does not declare)
/// is "method not found", so the server does not wait forever.
#[cfg(feature = "native")]
fn server_request_reply(method: &str, id: serde_json::Value) -> serde_json::Value {
    if method == "ping" {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
    } else {
        warn!("MCP server asked for '{method}', which this client does not support; declining");
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("method not supported by this client: {method}")}
        })
    }
}

/// The id of a response, numeric or the string form of one (a server may
/// echo the id back as a string).
#[cfg(feature = "native")]
fn response_id(value: &serde_json::Value) -> Option<u64> {
    match value.get("id")? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(feature = "native")]
#[async_trait]
impl McpTransport for StdioTransport {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let request_id = request.id;
        let method = request.method.clone();
        self.write_message(&serde_json::to_string(&request)?)
            .await?;

        // Read until this request's response. The server may interleave its
        // own messages: notifications (logging, progress, list_changed) are
        // skipped, requests (ping, sampling, roots) are answered, and a
        // response for another id — one whose caller timed out — is dropped.
        let mut stdout = self.stdout.lock().await;
        let mut non_json = 0usize;
        let mut last_non_json = String::new();
        loop {
            let mut line = String::new();
            let bytes_read = stdout
                .read_line(&mut line)
                .await
                .map_err(|e| McpError::Transport(format!("Read error: {}", e)))?;
            if bytes_read == 0 {
                let skipped = if non_json > 0 {
                    format!(
                        " ({non_json} non-JSON line(s) on stdout skipped; the last began: {last_non_json:?})"
                    )
                } else {
                    String::new()
                };
                return Err(self.closed_error(&skipped).await);
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    // Stray output (a banner, a debug print), the rest of a
                    // line a cancelled call left half-read — or the server's
                    // real answer, written broken. Say so: skipping it
                    // silently turns a broken answer into a timeout.
                    non_json += 1;
                    last_non_json = line.chars().take(80).collect();
                    warn!(
                        "MCP server '{}': skipping a non-JSON line on stdout during '{method}' ({} bytes, {e}): {last_non_json:?}",
                        self.command,
                        line.len()
                    );
                    continue;
                }
            };
            if let Some(server_method) = value.get("method").and_then(|m| m.as_str()) {
                match value.get("id").cloned() {
                    Some(id) => {
                        let reply = server_request_reply(server_method, id);
                        self.write_message(&reply.to_string()).await?;
                    }
                    None => debug!("MCP stdio: server notification '{server_method}'"),
                }
                continue;
            }
            match response_id(&value) {
                Some(id) if id == request_id => return Ok(serde_json::from_value(value)?),
                // JSON-RPC allows a null id on an error the server could not
                // attribute to a request — usually a line it could not parse.
                // Calls are one at a time, so it is taken as this one's;
                // logged, since an interrupted earlier write can cause it.
                None if value.get("error").is_some() => {
                    warn!(
                        "MCP server '{}': an error without an id arrived during '{method}'; reporting it for this call",
                        self.command
                    );
                    return Ok(serde_json::from_value(value)?);
                }
                other => debug!("MCP stdio: dropping a response for id {other:?} on '{method}'"),
            }
        }
    }

    async fn notify(&self, notification: JsonRpcNotification) -> Result<(), McpError> {
        self.write_message(&serde_json::to_string(&notification)?)
            .await
    }

    async fn close(&self) -> Result<(), McpError> {
        // Kill the server and reap it. An error here means it had already
        // exited, which is what close wants anyway: logged, not returned.
        let mut child = self.child.lock().await;
        if let Err(e) = child.kill().await {
            debug!("MCP server '{}': kill on close: {e}", self.command);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTTP Transport
// ---------------------------------------------------------------------------

/// Communicates with an MCP server via HTTP POST (JSON-RPC over HTTP).
///
/// Covers the **request/response subset of Streamable HTTP** — servers that
/// answer a POST with an SSE-framed response, whether or not they then close
/// the stream: `text/event-stream` bodies, `Mcp-Session-Id` capture and replay,
/// `202`/`204` acknowledgements, and session teardown on
/// [`close`](McpTransport::close). Plain JSON-RPC bodies keep working.
///
/// The body is parsed incrementally and [`send`](McpTransport::send) returns at
/// the blank-line-terminated frame carrying this request's response, so a
/// server that keeps the POST stream open after answering does not block the
/// call. Two consequences worth knowing: returning mid-body forgoes connection
/// reuse (a server that has not already closed the body costs a fresh
/// connection, and TLS handshake, next call), and a plain JSON-RPC body has no
/// frames to return early at, so it is read to the end as before.
///
/// A stalled server — one that accepts the POST and then sends nothing — is
/// bounded by an idle read timeout (120s) rather than hanging. The timer resets
/// on every read, so a slow-but-progressing call is never cut off.
///
/// Not covered: the `GET` server→client stream and `Last-Event-ID`
/// resumability. [`McpTransport`] is `send`/`close` only, so a server-initiated
/// message has nowhere to be delivered — supporting them would mean growing the
/// trait an inbound channel. Notifications that arrive on the POST stream
/// *before* the response are read and skipped; any that trail it are not, since
/// the call has already returned by then. A server that blocks awaiting a reply
/// to a `sampling/createMessage` it sent on this stream will therefore time out
/// rather than be answered.
pub struct HttpTransport {
    client: reqwest::Client,
    base_url: String,
    /// Session assigned by the server on `initialize`, replayed on subsequent
    /// requests. `Mutex` because [`McpTransport::send`] takes `&self`.
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    /// Idle bound between reads, not a bound on the whole call.
    ///
    /// A `tools/call` may legitimately run for minutes; what must not be
    /// tolerated is a server that accepts the POST and then sends *nothing*.
    /// `read_timeout` resets on every successful read, so a long call that
    /// streams progress frames keeps its connection alive while a stalled one
    /// is cut. (A whole-request `timeout` cannot tell those apart, which is why
    /// it is deliberately not used here.)
    #[cfg(not(target_arch = "wasm32"))]
    const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    /// Create a new HTTP transport.
    pub fn new(url: &str) -> Result<Self, McpError> {
        let builder = reqwest::Client::builder();
        // The fetch-based wasm client has no read timeout; the host's own
        // request limits apply there instead.
        #[cfg(not(target_arch = "wasm32"))]
        let builder = builder.read_timeout(Self::READ_IDLE_TIMEOUT);
        let client = builder
            .build()
            .map_err(|e| McpError::Transport(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: url.trim_end_matches('/').to_string(),
            session_id: Mutex::new(None),
        })
    }

    /// Decide whether `payload` is *this request's* JSON-RPC response.
    ///
    /// Selection is structural rather than "does it deserialize". Every field
    /// of [`JsonRpcResponse`] except `jsonrpc` is optional and unknown keys are
    /// ignored, so a server→client notification like
    /// `{"jsonrpc":"2.0","method":"notifications/progress",...}` deserializes
    /// cleanly into an all-`None` shell. Streamable HTTP servers routinely emit
    /// progress and logging notifications on the POST stream *ahead of* the
    /// result, so accepting the first thing that parses would return the
    /// notification and silently discard the answer.
    fn response_for(payload: &str, request_id: u64) -> Option<JsonRpcResponse> {
        let value: serde_json::Value = serde_json::from_str(payload).ok()?;
        // Responses never carry `method`. The result-or-error check below
        // happens to reject every *well-formed* frame this one does; what this
        // still catches is a malformed frame carrying both — a server that
        // conflates the two shapes. Cheap insurance, and pinned by
        // `frame_with_both_method_and_result_is_not_the_response`.
        if value.get("method").is_some() {
            return None;
        }
        // A response carries a result or an error. This is the check that also
        // rejects a bare `{"jsonrpc":"2.0","id":N}` ack, which carries no
        // method and would otherwise pass.
        if value.get("result").is_none() && value.get("error").is_none() {
            return None;
        }
        let response: JsonRpcResponse = serde_json::from_value(value).ok()?;
        // Correlate. An absent id is tolerated: JSON-RPC allows a null id on
        // errors the server could not attribute to a request.
        if response.id.is_some_and(|id| id != request_id) {
            return None;
        }
        Some(response)
    }

    /// Join an SSE event's `data:` lines into its payload, per the SSE spec.
    ///
    /// Returns `None` for events that carry no data — comments, bare `event:`
    /// or `id:` lines, keep-alives.
    fn event_payload(event: &str) -> Option<String> {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data:")
                    .map(|d| d.trim_start_matches(' '))
            })
            .collect();
        if data.is_empty() {
            return None;
        }
        let payload = data.join("\n");
        let payload = payload.trim().to_string();
        (!payload.is_empty()).then_some(payload)
    }

    /// Note a frame that was valid JSON-RPC but not our answer.
    ///
    /// Counted by kind rather than collected verbatim: this ends up in an error
    /// string that `McpToolAdapter` hands to the model as a tool result, and a
    /// progress-heavy stream can emit thousands of identical frame names.
    /// Reaches a caller only on the EOF path — a mid-stream read error reports
    /// its own cause instead.
    fn note_skipped(payload: &str, method: &str, skipped: &mut BTreeMap<String, usize>) {
        let what = match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(value) => match value.get("method").and_then(|m| m.as_str()) {
                Some(m) => m.to_string(),
                None => match value.get("id").and_then(|i| i.as_u64()) {
                    Some(id) => format!("response for id {id}"),
                    None => "unrecognized JSON-RPC frame".to_string(),
                },
            },
            // No `else`-less `if let` here: a frame that is not valid JSON may
            // be the server's own answer, truncated mid-write. Dropping it
            // unrecorded is how that becomes undiagnosable.
            Err(e) => {
                warn!(
                    "SSE frame on '{method}' is not valid JSON ({} bytes): {e}; \
                     a truncated frame may be the server's real answer",
                    payload.len()
                );
                format!("malformed non-JSON frame ({} bytes)", payload.len())
            }
        };
        debug!("skipping SSE frame that is not the response to '{method}': {what}");
        *skipped.entry(what).or_default() += 1;
    }

    /// Render the skipped-frame tally for an error message.
    fn describe_skipped(skipped: &BTreeMap<String, usize>) -> String {
        if skipped.is_empty() {
            return String::new();
        }
        let total: usize = skipped.values().sum();
        let listed: Vec<String> = skipped
            .iter()
            .take(10)
            .map(|(what, n)| {
                if *n > 1 {
                    format!("{what} x{n}")
                } else {
                    what.clone()
                }
            })
            .collect();
        let more = skipped.len().saturating_sub(listed.len());
        let tail = if more > 0 {
            format!(", and {more} other kind(s)")
        } else {
            String::new()
        };
        format!(" (skipped {total} frame(s): {}{tail})", listed.join(", "))
    }

    /// Decode an event slice, refusing rather than substituting on bad UTF-8.
    ///
    /// Lossy decoding would replace invalid bytes with U+FFFD, and since that
    /// is legal JSON string content the frame would go on to parse and be
    /// returned as a successful — but silently mutated — tool result.
    fn decode(bytes: &[u8], method: &str) -> Result<String, McpError> {
        std::str::from_utf8(bytes).map(str::to_owned).map_err(|e| {
            McpError::Transport(format!(
                "invalid UTF-8 in the response body on '{method}' at byte {}: {e}",
                e.valid_up_to()
            ))
        })
    }

    /// Read the response body, returning as soon as this request's answer
    /// arrives rather than draining to EOF.
    ///
    /// The early return is the point: Streamable HTTP permits a server to keep
    /// the POST stream open after answering, so buffering the whole body would
    /// block until the server gave up. It also means a long `tools/call` that
    /// streams progress frames returns the moment the result lands, instead of
    /// waiting out the trailing traffic.
    ///
    /// Two costs come with it, both deliberate. Returning mid-body prevents the
    /// connection from being pooled, so a server that has not already closed
    /// the body costs a fresh connection (and TLS handshake) on the next call.
    /// And the early return only fires on a **blank-line-terminated** frame: an
    /// unterminated final event is recovered at EOF instead, so a server that
    /// neither terminates the frame nor closes the stream is bounded by the
    /// client's read timeout rather than returning promptly.
    ///
    /// A plain JSON-RPC body yields no `data:`-prefixed lines — a raw newline is
    /// illegal inside a JSON string, so no line can begin mid-string — and so
    /// never produces an event payload. It falls through to the whole-body parse
    /// at EOF. Note this reverses the previous ordering: JSON bodies now
    /// traverse the SSE scan first.
    async fn read_response(
        resp: reqwest::Response,
        request_id: u64,
        method: &str,
        status: reqwest::StatusCode,
    ) -> Result<JsonRpcResponse, McpError> {
        // Both `application/json` and `text/event-stream` mandate UTF-8, and
        // reading the body as a byte stream gives up the charset transcoding
        // `Response::text()` would have done. Refuse a declared non-UTF-8
        // charset rather than hand back mangled text.
        if let Some(charset) = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| {
                ct.split(';').skip(1).find_map(|p| {
                    p.trim()
                        .strip_prefix("charset=")
                        .map(|c| c.trim_matches('"').to_ascii_lowercase())
                })
            })
        {
            if !matches!(charset.as_str(), "utf-8" | "utf8" | "us-ascii" | "ascii") {
                return Err(McpError::Transport(format!(
                    "unsupported charset '{charset}' on '{method}': MCP bodies are UTF-8 \
                     (both application/json and text/event-stream mandate it)"
                )));
            }
        }

        // Scanning happens on bytes, not text: a chunk boundary can split a
        // multi-byte character, and decoding each chunk independently would
        // corrupt it. Whole events decode cleanly, and 0x0D can never appear
        // inside a multi-byte sequence (continuation bytes are >= 0x80), which
        // is what makes the CR normalization below safe to do pre-decode.
        //
        // `buf` is never compacted — `scanned` is a read cursor — so a stream
        // of discarded progress frames is retained for the life of the call.
        let mut buf: Vec<u8> = Vec::new();
        let mut scanned = 0usize;
        // Boundary-free below this point; a new 2-byte window can only be
        // completed by a newly-appended byte. Without it, a body with no
        // boundary (a plain JSON body, or one large SSE frame) rescans
        // everything on every chunk — quadratic, and seconds of CPU on a
        // multi-megabyte result.
        let mut searched = 0usize;
        let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
        let mut pending_cr = false;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                McpError::Transport(format!(
                    "Response read error on '{method}' after {} byte(s){}: {e}",
                    buf.len(),
                    Self::describe_skipped(&skipped)
                ))
            })?;

            // SSE accepts CRLF, LF, or a bare CR as a line terminator.
            // Normalize all three to LF so boundaries are plain `\n\n`.
            // `pending_cr` carries the state across a chunk that ends mid-CRLF.
            // Within a JSON payload a raw CR is legal only as inter-token
            // whitespace (it is illegal unescaped inside a string, and a `\r`
            // escape is two ASCII bytes), so rewriting it cannot alter a value.
            for &b in chunk.iter() {
                if b == b'\r' {
                    buf.push(b'\n');
                    pending_cr = true;
                } else {
                    if pending_cr && b == b'\n' {
                        pending_cr = false;
                        continue;
                    }
                    pending_cr = false;
                    buf.push(b);
                }
            }

            loop {
                let from = scanned.max(searched);
                let Some(pos) = buf[from..].windows(2).position(|w| w == b"\n\n") else {
                    // The trailing byte may yet start a boundary.
                    searched = buf.len().saturating_sub(1);
                    break;
                };
                let end = from + pos;
                let event = Self::decode(&buf[scanned..end], method)?;
                scanned = end + 2;
                searched = scanned;

                let Some(payload) = Self::event_payload(&event) else {
                    continue;
                };
                if let Some(response) = Self::response_for(&payload, request_id) {
                    return Ok(response);
                }
                Self::note_skipped(&payload, method, &mut skipped);
            }
        }

        let body = Self::decode(&buf, method)?;
        // `Response::text()` used to strip a BOM; `from_utf8` does not, and
        // U+FEFF is not whitespace, so `trim()` leaves it in place to break the
        // JSON parse below.
        let body = body.strip_prefix('\u{feff}').unwrap_or(&body).to_string();

        if body.trim().is_empty() {
            // 202/204 with no body is how Streamable HTTP acknowledges a
            // notification — there is no JSON-RPC response to return, so
            // synthesize an empty success. Any other empty 2xx is a real
            // failure (a proxy answering instead of the MCP server, a drained
            // upstream) and must not be dressed up as one.
            if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT
            {
                return Ok(JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: Some(request_id),
                    result: None,
                    error: None,
                });
            }
            return Err(McpError::Transport(format!(
                "HTTP {status} with an empty body on '{method}' (expected a JSON-RPC response; \
                 an empty 2xx usually means a proxy or gateway answered instead of the MCP server)"
            )));
        }

        // Plain JSON-RPC: never yields a `data:` line, so nothing matched above.
        if let Some(response) = Self::response_for(&body, request_id) {
            return Ok(response);
        }

        // A final event the server never terminated with a blank line.
        if scanned < buf.len() {
            let tail = Self::decode(&buf[scanned..], method)?;
            if let Some(payload) = Self::event_payload(&tail) {
                if let Some(response) = Self::response_for(&payload, request_id) {
                    return Ok(response);
                }
                Self::note_skipped(&payload, method, &mut skipped);
            }
        }

        Err(McpError::Transport(format!(
            "HTTP {status}: no JSON-RPC response for '{method}' (id {request_id}) in the body{}: {}",
            Self::describe_skipped(&skipped),
            body.chars().take(200).collect::<String>()
        )))
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl McpTransport for HttpTransport {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let request_id = request.id;
        let method = request.method.clone();

        let mut builder = self
            .client
            .post(&self.base_url)
            // Streamable HTTP servers pick their framing from this; JSON-only
            // servers still match `application/json`.
            .header("Accept", "application/json, text/event-stream")
            .json(&request);

        if let Some(session) = self.session_id.lock().await.as_ref() {
            builder = builder.header("Mcp-Session-Id", session);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| McpError::Transport(format!("HTTP error: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            // Per the spec, 404 on a request carrying a session means the
            // server dropped it. Keeping the dead id would make every later
            // call fail identically, with an error that reads like a bad URL.
            if status == reqwest::StatusCode::NOT_FOUND {
                if let Some(dead) = self.session_id.lock().await.take() {
                    warn!(
                        "MCP session {dead} was rejected (HTTP 404); reconnect to start a new one"
                    );
                    return Err(McpError::Transport(format!(
                        "MCP session expired (HTTP 404 on '{method}'); reconnect to start a new session"
                    )));
                }
            }
            // Carry the body: servers explain themselves in it, and dropping it
            // leaves the caller with a bare status to guess from.
            let body = resp.text().await.unwrap_or_default();
            let detail = body.trim();
            return Err(McpError::Transport(if detail.is_empty() {
                format!("HTTP {status} from server on '{method}'")
            } else {
                format!(
                    "HTTP {status} from server on '{method}': {}",
                    detail.chars().take(200).collect::<String>()
                )
            }));
        }

        // Servers assign the session on `initialize`; any response carrying one
        // updates it.
        match resp.headers().get("mcp-session-id").map(|v| v.to_str()) {
            Some(Ok(session)) => *self.session_id.lock().await = Some(session.to_owned()),
            // A header we cannot read means every later request goes out
            // sessionless — the server then rejects them or silently starts a
            // fresh session, discarding the handshake. Only the operator can
            // fix that, so say so.
            Some(Err(e)) => warn!("ignoring unreadable Mcp-Session-Id header: {e}"),
            None => {}
        }

        Self::read_response(resp, request_id, &method, status).await
    }

    async fn notify(&self, notification: JsonRpcNotification) -> Result<(), McpError> {
        let method = notification.method.clone();
        let mut builder = self
            .client
            .post(&self.base_url)
            .header("Accept", "application/json, text/event-stream")
            .json(&notification);
        if let Some(session) = self.session_id.lock().await.as_ref() {
            builder = builder.header("Mcp-Session-Id", session);
        }
        // A notification is acknowledged with 202 and no body; nothing waits
        // for anything after the status.
        let resp = builder
            .send()
            .await
            .map_err(|e| McpError::Transport(format!("HTTP error on '{method}': {e}")))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            let detail = body.trim();
            Err(McpError::Transport(if detail.is_empty() {
                format!("HTTP {status} from server on notification '{method}'")
            } else {
                format!(
                    "HTTP {status} from server on notification '{method}': {}",
                    detail.chars().take(200).collect::<String>()
                )
            }))
        }
    }

    async fn close(&self) -> Result<(), McpError> {
        let session = self.session_id.lock().await.take();
        if let Some(session) = session {
            // Best-effort: session teardown is optional in the spec and plenty
            // of servers reject DELETE. Failing close() over it would turn a
            // successful run into an error. Best-effort is not the same as
            // unobservable, though — a rejection is fine, but a DELETE that
            // never reached the server leaks the session there, and that only
            // surfaces later as an unrelated connect failure.
            match self
                .client
                .delete(&self.base_url)
                .header("Mcp-Session-Id", &session)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => debug!(
                    "MCP session teardown rejected with HTTP {}; it is optional, continuing",
                    resp.status()
                ),
                Err(e) => warn!(
                    "MCP session {session} teardown did not reach {}: {e}; \
                     the session may leak server-side",
                    self.base_url
                ),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "native")]
    #[tokio::test]
    async fn test_stdio_transport_with_cat() {
        // Use `cat` as a simple echo server — it reflects stdin to stdout.
        let transport = StdioTransport::new("cat", &[], None).await.unwrap();

        let request = JsonRpcRequest::new("test/echo", Some(serde_json::json!({"hello": "world"})));
        let request_id = request.id;

        // Write the request; cat will echo it back as-is.
        // Since cat echoes JSON-RPC requests, the "response" will actually be the request.
        // This tests the transport layer I/O, not protocol correctness.
        let mut line = serde_json::to_string(&request).unwrap();
        line.push('\n');

        {
            let mut writer = transport.stdin.lock().await;
            writer.stdin.write_all(line.as_bytes()).await.unwrap();
            writer.stdin.flush().await.unwrap();
        }

        let mut response_line = String::new();
        {
            let mut stdout = transport.stdout.lock().await;
            stdout.read_line(&mut response_line).await.unwrap();
        }

        // Cat echoes the request, so we can parse it as a request
        let echoed: JsonRpcRequest = serde_json::from_str(response_line.trim()).unwrap();
        assert_eq!(echoed.id, request_id);
        assert_eq!(echoed.method, "test/echo");

        transport.close().await.unwrap();
    }

    #[test]
    fn test_http_transport_creation() {
        let transport = HttpTransport::new("http://localhost:8080/mcp").unwrap();
        assert_eq!(transport.base_url, "http://localhost:8080/mcp");

        // Trailing slash stripped
        let transport = HttpTransport::new("http://localhost:8080/mcp/").unwrap();
        assert_eq!(transport.base_url, "http://localhost:8080/mcp");
    }
}
