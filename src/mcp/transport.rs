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
    /// waits for an answer, best-effort (an error is logged, not returned) —
    /// what this crate did before notifications had their own path. A
    /// spec-compliant server never answers a notification, so a transport
    /// should override this (the built-in ones do) to write it and return;
    /// an override's error fails the handshake.
    async fn notify(&self, notification: JsonRpcNotification) -> Result<(), McpError> {
        let method = notification.method.clone();
        if let Err(e) = self
            .send(JsonRpcRequest::new(
                notification.method,
                notification.params,
            ))
            .await
        {
            warn!("MCP notification '{method}' through send() failed: {e}");
        }
        Ok(())
    }
    /// Close the transport.
    async fn close(&self) -> Result<(), McpError>;

    /// Report the server notifications this transport reads to `to`
    /// ([`ServerNotifications::record`] each one's method), and return
    /// whether it will. The client calls this once, when it is built; what it
    /// learns backs [`McpClient::tools_list_changes`](crate::mcp::McpClient::tools_list_changes)
    /// and an [`McpToolSource`](crate::mcp::McpToolSource)'s generation.
    ///
    /// The default reports nothing and returns `false`, so a custom
    /// transport's server is taken never to change its tools
    /// ([`McpClient::reports_notifications`](crate::mcp::McpClient::reports_notifications)
    /// says so). The built-in transports report the notifications they read
    /// while a call is answered.
    fn report_notifications(&mut self, to: ServerNotifications) -> bool {
        let _ = to;
        false
    }

    /// A way to tell, without waiting, that the server has gone — read by
    /// [`McpClient::exited`](crate::mcp::McpClient::exited) outside the lock
    /// a call holds, so it never waits behind one. The client asks once,
    /// when it is built.
    ///
    /// The default is `None`: nothing to watch (the HTTP transport — a
    /// remote server has no process here — and custom transports, unless
    /// they implement it). The stdio transport watches its child process.
    fn exit_watch(&self) -> Option<ExitWatch> {
        None
    }
}

/// Tells whether an MCP server has gone, without waiting (see
/// [`McpTransport::exit_watch`]). Cheap to clone; clones share the check.
#[derive(Clone)]
pub struct ExitWatch {
    check: std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl std::fmt::Debug for ExitWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExitWatch").finish_non_exhaustive()
    }
}

impl ExitWatch {
    /// A watch over `check`: `None` while the server runs, then why it went.
    /// It must not block (it is polled, and read every turn by an
    /// [`McpToolSource`](crate::mcp::McpToolSource)), and once it has
    /// answered `Some` it must keep doing so (the text may still change).
    pub fn new(check: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            check: std::sync::Arc::new(check),
        }
    }

    /// `None` while the server runs; once it has gone, why (for stdio: its
    /// exit status and the last of its stderr). `Some` from then on.
    pub fn exited(&self) -> Option<String> {
        (self.check)()
    }
}

/// The notification a server sends when its tool list changed.
const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";

/// Where a transport reports the server notifications it reads (see
/// [`McpTransport::report_notifications`]). Cheap to clone; clones share the
/// counts.
#[derive(Clone, Debug, Default)]
pub struct ServerNotifications {
    tools_list_changed: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ServerNotifications {
    /// A fresh handle, nothing recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a notification the server sent, by its JSON-RPC `method`.
    /// Methods the client does not act on are ignored.
    pub fn record(&self, method: &str) {
        if method == TOOLS_LIST_CHANGED {
            self.tools_list_changed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            debug!("MCP server: its tool list changed");
        }
    }

    /// How many `notifications/tools/list_changed` were recorded.
    pub fn tools_list_changed(&self) -> u64 {
        self.tools_list_changed
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Record a notification on `to`, when the transport has been given one.
fn note_notification(method: &str, to: Option<&ServerNotifications>) {
    if let Some(to) = to {
        to.record(method);
    }
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
/// dropped: the command alone, or its whole process group with
/// [`StdioOptions::own_process_group`] (see [`with_options`](Self::with_options)).
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
    /// Where server notifications are reported (see
    /// [`McpTransport::report_notifications`]).
    notifications: Option<ServerNotifications>,
    /// What the exit watch needs besides the process (see
    /// [`McpTransport::exit_watch`]).
    ended: Arc<Ended>,
    /// The server's own process group ([`StdioOptions::own_process_group`]):
    /// killed whole, once, on `close` or drop — while its leader is unreaped.
    group: Arc<std::sync::Mutex<Option<u32>>>,
}

/// How [`StdioTransport::with_options`] starts the server. Native hosts only.
#[cfg(feature = "native")]
#[cfg_attr(docsrs, doc(cfg(feature = "native")))]
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct StdioOptions {
    own_process_group: bool,
}

#[cfg(feature = "native")]
impl StdioOptions {
    /// The defaults: the server runs in the host's process group.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run the server in a process group of its own (Unix; ignored
    /// elsewhere):
    /// - a Ctrl+C (or another signal) sent to the host's terminal no longer
    ///   reaches it — the host decides when it stops;
    /// - [`close`](McpTransport::close) and dropping the transport kill the
    ///   whole group: what the server started (`npx` → `node`, `uvx` →
    ///   `python`) goes with it, where by default only the command itself is
    ///   killed and a child that ignores its closed input lives on;
    /// - so does the command exiting on its own, as soon as it is seen (the
    ///   exit watch, or a call finding its output closed): the server is
    ///   gone, and what it left in its group has no client. A launcher that
    ///   backgrounds the real server on its own pipes and exits takes that
    ///   server down too — it reads as gone the moment the launcher exits.
    ///
    /// The costs:
    /// - when the host exits without dropping its clients — killed, an
    ///   uncaught Ctrl+C, `process::exit`, an abort — whatever in the group
    ///   ignores its closed input keeps running, where a terminal's Ctrl+C or
    ///   hangup would have reached it in the host's group. A terminal host
    ///   should catch Ctrl+C and drop (or close) its clients;
    /// - the group is signalled only while the command itself is still the
    ///   transport's — running, or exited and not yet collected — so its id
    ///   is safe to signal; under a host that ignores SIGCHLD the command is
    ///   reaped unseen, and its group is left alone.
    pub fn own_process_group(mut self, yes: bool) -> Self {
        self.own_process_group = yes;
        self
    }
}

/// How a stdio server's end shows, besides its process exiting.
#[cfg(feature = "native")]
#[derive(Default)]
struct Ended {
    /// A call found its output closed (a wrapper can outlive the server).
    output_closed: std::sync::atomic::AtomicBool,
    /// A write to its input failed: it no longer reads it.
    input_closed: std::sync::atomic::AtomicBool,
    /// `close` stopped it: not a crash.
    closed_by_host: std::sync::atomic::AtomicBool,
    /// The stderr drain read to the end: the tail is complete.
    stderr_done: std::sync::atomic::AtomicBool,
}

/// How long, after an exit is first seen, its answer may still change (gain
/// the last of the server's stderr) before it is latched.
#[cfg(feature = "native")]
const EXIT_TAIL_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

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
        Self::with_options(command, args, env, StdioOptions::default()).await
    }

    /// [`new`](Self::new), started as `options` says (a process group of
    /// its own, say).
    pub async fn with_options(
        command: &str,
        args: &[&str],
        env: Option<HashMap<String, String>>,
        options: StdioOptions,
    ) -> Result<Self, McpError> {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cmd = std::process::Command::new(command);
        // std's `process_group` (Rust 1.64) on a std command, converted: tokio's
        // own `process_group` needs tokio 1.40 and `as_std_mut` 1.39, newer than
        // the `tokio = "1"` this crate asks for.
        #[cfg(unix)]
        if options.own_process_group {
            std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        }
        let mut cmd = Command::from(cmd);
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
        // The group's id is the leader's pid.
        let group = if cfg!(unix) && options.own_process_group {
            child.id()
        } else {
            None
        };

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("Failed to capture stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("Failed to capture stdout".into()))?;

        let stderr_tail = Arc::new(std::sync::Mutex::new(StderrTail::default()));
        let ended = Arc::new(Ended::default());
        let stderr_drain = match child.stderr.take() {
            Some(stderr) => {
                let (server, tail, ended) =
                    (command.to_string(), stderr_tail.clone(), ended.clone());
                Some(tokio::spawn(async move {
                    drain_stderr(stderr, server, tail).await;
                    ended
                        .stderr_done
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }))
            }
            None => {
                ended
                    .stderr_done
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                None
            }
        };

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
            notifications: None,
            ended,
            group: Arc::new(std::sync::Mutex::new(group)),
        })
    }

    /// Kill the server's own process group, everything in it (with
    /// [`StdioOptions::own_process_group`]) — once, and only while its leader
    /// (`child`, the server's command) is still ours: running, or exited and
    /// not yet collected (a zombie). Either way it holds its pid, the
    /// group's id, so the signal cannot reach an unrelated group. Asked
    /// without collecting it (`waitid` with `WNOWAIT`), so a wrapper that
    /// exited uncollected still has its children killed. A leader already
    /// collected — by a call or the exit watch — or not ours any more (a
    /// host ignoring SIGCHLD has it reaped unseen) leaves its group alone.
    fn kill_group(&self, child: &mut Child) {
        kill_group(&self.group, child, &self.command);
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
        let input_closed = |what: &str, e: std::io::Error| {
            // It no longer reads its input: gone, though its process may
            // live on (see the exit watch).
            self.ended
                .input_closed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            McpError::Transport(format!("{what} error: {e}"))
        };
        writer
            .stdin
            .write_all(message.as_bytes())
            .await
            .map_err(|e| input_closed("Write", e))?;
        writer
            .stdin
            .flush()
            .await
            .map_err(|e| input_closed("Flush", e))?;
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
        // Stdout can close a moment before the process is reaped; wait
        // briefly for its exit status rather than report it as still running.
        let status = {
            let mut child = self.child.lock().await;
            let own_group = self
                .group
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some();
            let exited = if own_group
                && self
                    .ended
                    .output_closed
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                // An earlier call waited its window: it runs on.
                None
            } else if own_group {
                // One 500 ms window, without collecting a running leader: its
                // exit is seen first (its group killed), then it is collected.
                let started = std::time::Instant::now();
                let mut running = true;
                while started.elapsed() < std::time::Duration::from_millis(500) {
                    if settle_group(&self.group, &mut child, &self.command) != Some(false) {
                        running = false;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                if running {
                    None
                } else {
                    tokio::time::timeout(std::time::Duration::from_millis(500), child.wait())
                        .await
                        .ok()
                        .and_then(Result::ok)
                }
            } else {
                tokio::time::timeout(std::time::Duration::from_millis(500), child.wait())
                    .await
                    .ok()
                    .and_then(Result::ok)
            };
            match exited {
                Some(status) => format!("exited with {status}"),
                None => {
                    // Still running after the wait: a wrapper outliving the
                    // server (the exit watch reports it as gone).
                    self.ended
                        .output_closed
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    "closed its output".to_string()
                }
            }
        };
        let tail = stderr_suffix(&self.stderr_tail);
        McpError::Transport(format!(
            "Connection closed: MCP server '{}' {status}{skipped}{tail}",
            self.command
        ))
    }
}

/// Kill `group` (taken once) while its leader `child` is still ours — see
/// [`StdioTransport::kill_group`]. Shared with the exit watch, which kills it
/// when it first sees the leader exited, before collecting it.
#[cfg(feature = "native")]
fn kill_group(group: &std::sync::Mutex<Option<u32>>, child: &mut Child, command: &str) {
    let Some(group) = group.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    // `None` once tokio collected it.
    let Some(pid) = child.id() else {
        debug!(
            "MCP server '{command}': its command already exited and was collected; its process group is left alone"
        );
        return;
    };
    #[cfg(unix)]
    if let (Ok(pgid), Some(_)) = (libc::pid_t::try_from(group), leader_state(pid)) {
        // SAFETY: `killpg` takes two integers and touches no memory of
        // ours. The leader is ours and uncollected (just asked, under the
        // lock): the id is still its group's.
        if unsafe { libc::killpg(pgid, libc::SIGKILL) } == -1 {
            let e = std::io::Error::last_os_error();
            // ESRCH: the group is already gone.
            if e.raw_os_error() != Some(libc::ESRCH) {
                debug!("MCP server '{command}': killing its process group: {e}");
            }
        }
    } else {
        debug!("MCP server '{command}': its command is no longer ours to signal; its process group is left alone");
    }
    #[cfg(not(unix))]
    let _ = (group, pid);
}

/// The leader `pid` as `waitid` sees it without collecting it (`WNOWAIT`):
/// `Some(true)` exited (a zombie, its pid still held), `Some(false)`
/// running, `None` not ours (collected, or reaped unseen under an ignored
/// SIGCHLD).
#[cfg(all(feature = "native", unix))]
fn leader_state(pid: u32) -> Option<bool> {
    let pid = libc::id_t::try_from(pid).ok()?;
    // SAFETY: `info` is a zeroed `siginfo_t` we own, written by `waitid`
    // only; `WNOWAIT` leaves a zombie for its owner to collect, `WNOHANG`
    // never blocks. `si_signo` stays 0 while the child runs (the struct
    // was zeroed; some systems do not fill it then) and is SIGCHLD once
    // it has exited.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let ok = libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0;
        ok.then_some(info.si_signo == libc::SIGCHLD)
    }
}

/// With an own process group still to kill: where the leader stands, its
/// group handled first. `Some(true)`: it exited, its group was killed — now
/// collect it. `Some(false)`: it runs — do not collect it (a collect racing
/// its exit would free its id with the group left). `None`: no group to
/// handle, or the leader cannot be judged — collect as usual.
#[cfg(feature = "native")]
fn settle_group(
    group: &std::sync::Mutex<Option<u32>>,
    child: &mut Child,
    command: &str,
) -> Option<bool> {
    if group.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
        return None;
    }
    #[cfg(unix)]
    {
        let exited = child.id().and_then(leader_state)?;
        if exited {
            kill_group(group, child, command);
        }
        Some(exited)
    }
    #[cfg(not(unix))]
    {
        let _ = (child, command);
        None
    }
}

/// `; last stderr:` and the kept tail, or nothing when the server wrote none.
#[cfg(feature = "native")]
fn stderr_suffix(tail: &std::sync::Mutex<StderrTail>) -> String {
    let tail = tail.lock().map(|t| t.render()).unwrap_or_default();
    if tail.trim().is_empty() {
        String::new()
    } else {
        format!("; last stderr:\n{tail}")
    }
}

#[cfg(feature = "native")]
impl Drop for StdioTransport {
    fn drop(&mut self) {
        if self
            .group
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return;
        }
        // Before the child (`kill_on_drop` kills the command alone). Only
        // an exit-watch poll can hold the lock now (calls and `close`
        // borrow the transport), and only for a `try_wait`: wait for it
        // briefly — `blocking_lock` would panic inside a runtime.
        for _ in 0..1000 {
            if let Ok(mut child) = self.child.try_lock() {
                self.kill_group(&mut child);
                return;
            }
            std::thread::yield_now();
        }
        warn!(
            "MCP server '{}': its process stayed locked at drop; its process group was not \
             killed (what it started may still run)",
            self.command
        );
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
        // own messages: notifications (logging, progress) are skipped —
        // `tools/list_changed` is counted first — requests (ping, sampling, roots) are answered, and a
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
                    None => {
                        debug!("MCP stdio: server notification '{server_method}'");
                        note_notification(server_method, self.notifications.as_ref());
                    }
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

    fn report_notifications(&mut self, to: ServerNotifications) -> bool {
        self.notifications = Some(to);
        true
    }

    fn exit_watch(&self) -> Option<ExitWatch> {
        // Weak: a watch kept after its client is dropped must not keep the
        // process alive (it is killed when the transport goes).
        let child = Arc::downgrade(&self.child);
        let (tail, ended, command, group) = (
            self.stderr_tail.clone(),
            self.ended.clone(),
            self.command.clone(),
            self.group.clone(),
        );
        // Gone is reported at once; the answer settles (the reason looked up
        // again, the stderr tail read again) until the drain has read the
        // tail or `EXIT_TAIL_GRACE` since first sight, then it is latched.
        let first_seen = std::sync::Mutex::new(None::<std::time::Instant>);
        // The reason last seen, without the stderr tail (added once below).
        let provisional = std::sync::Mutex::new(None::<String>);
        let latched = std::sync::Mutex::new(None::<String>);
        Some(ExitWatch::new(move || {
            use std::sync::atomic::Ordering::SeqCst;
            let mut latched = latched.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(why) = latched.as_ref() {
                return Some(why.clone());
            }
            let mut provisional = provisional.lock().unwrap_or_else(|p| p.into_inner());
            let why = if ended.closed_by_host.load(SeqCst) {
                Some(format!("MCP server '{command}' was closed (`close`)"))
            } else if let Some(child) = child.upgrade() {
                // Held only while a call reaps a server whose output closed,
                // by `close`, or by the transport's drop: the last answer, or
                // not known yet.
                match child.try_lock() {
                    Err(_) => provisional.clone(),
                    Ok(mut child) => {
                        // With an own group: an exited leader's group goes
                        // before `try_wait` collects it (its id is then
                        // free); a running one is not collected at all.
                        let state = match settle_group(&group, &mut child, &command) {
                            Some(false) => Ok(None),
                            _ => child.try_wait(),
                        };
                        match state {
                        Ok(Some(status)) => {
                            Some(format!("MCP server '{command}' exited with {status}"))
                        }
                        Ok(None) if ended.output_closed.load(SeqCst) => Some(format!(
                            "MCP server '{command}' closed its output (its process is still running)"
                        )),
                        Ok(None) if ended.input_closed.load(SeqCst) => Some(format!(
                            "MCP server '{command}' no longer reads its input (its process is still running)"
                        )),
                        Ok(None) => None,
                        // Not waitable (`ECHILD`: SIGCHLD ignored, or reaped
                        // elsewhere): it will not answer again either.
                        Err(e) => Some(format!(
                            "MCP server '{command}' has gone (exit status unknown: {e})"
                        )),
                        }
                    }
                }
            } else {
                // Its transport was dropped, killing it: nothing to wait for.
                let why = format!("MCP server '{command}' was closed: its client was dropped");
                *latched = Some(why.clone());
                return Some(why);
            };
            let why = why?;
            *provisional = Some(why.clone());
            // Before the tail is read: the drain pushes its last line, then
            // sets `stderr_done`, so a tail read after seeing it is complete.
            let done = ended.stderr_done.load(SeqCst);
            let why = format!("{why}{}", stderr_suffix(&tail));
            let first = *first_seen
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_or_insert_with(std::time::Instant::now);
            if done || first.elapsed() >= EXIT_TAIL_GRACE {
                *latched = Some(why.clone());
            }
            Some(why)
        }))
    }

    async fn close(&self) -> Result<(), McpError> {
        self.ended
            .closed_by_host
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Kill the server and reap it. An error here means it had already
        // exited, which is what close wants anyway: logged, not returned.
        let mut child = self.child.lock().await;
        self.kill_group(&mut child);
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
///
/// Extra request headers — an auth token, a `User-Agent` — are set with
/// [`with_header`](Self::with_header) and go on every request of the session.
pub struct HttpTransport {
    client: reqwest::Client,
    base_url: String,
    /// The caller's headers, on every request. Values are marked sensitive.
    headers: reqwest::header::HeaderMap,
    /// Session assigned by the server on `initialize`, replayed on subsequent
    /// requests. `Mutex` because [`McpTransport::send`] takes `&self`.
    session_id: Mutex<Option<String>>,
    /// Where notifications read on a POST stream are reported (see
    /// [`McpTransport::report_notifications`]).
    notifications: Option<ServerNotifications>,
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
            headers: reqwest::header::HeaderMap::new(),
            session_id: Mutex::new(None),
            notifications: None,
        })
    }

    /// Headers the transport or the HTTP client sets itself; a caller's header
    /// of the same name would break the protocol or the request framing, so
    /// [`with_header`](Self::with_header) refuses them.
    const RESERVED_HEADERS: [&'static str; 7] = [
        "accept",
        "content-type",
        "mcp-session-id",
        "content-length",
        "transfer-encoding",
        "connection",
        "host",
    ];

    /// Send `name: value` on every request of the session: `initialize`, the
    /// `initialized` notification, tool calls and the closing `DELETE`.
    ///
    /// Setting the same name again replaces the value. The value is marked
    /// sensitive, so it stays out of debug output. An invalid name, a value
    /// that is not visible ASCII, or a name the transport or HTTP client sets
    /// itself (`Accept`, `Content-Type`, `Mcp-Session-Id`, `Content-Length`,
    /// `Transfer-Encoding`, `Connection`, `Host`) is an error here, before
    /// anything is sent. The headers are fixed for the transport's lifetime;
    /// a token that changes during a session needs a new transport.
    ///
    /// ```no_run
    /// # async fn run(token: &str) -> Result<(), yoagent::mcp::McpError> {
    /// use yoagent::mcp::{HttpTransport, McpClient};
    /// let transport = HttpTransport::new("https://mcp.example.com/mcp")?
    ///     .with_header("authorization", format!("Bearer {token}"))?;
    /// let client = McpClient::connect_http_with(transport).await?;
    /// # Ok(()) }
    /// ```
    pub fn with_header(mut self, name: &str, value: impl AsRef<str>) -> Result<Self, McpError> {
        let header = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| McpError::Transport(format!("invalid header name {name:?}: {e}")))?;
        if Self::RESERVED_HEADERS.contains(&header.as_str()) {
            return Err(McpError::Transport(format!(
                "header {name:?} is set by the MCP transport or HTTP client and cannot be overridden"
            )));
        }
        // The value may be a secret: name the header, never echo the value.
        // Visible ASCII only: `HeaderValue` also takes bytes 0x80-0xFF, which
        // many servers reject and wasm32's fetch-based client refuses on every
        // request — refuse them here instead.
        let invalid = || McpError::Transport(format!("invalid value for header {name:?}"));
        let mut value =
            reqwest::header::HeaderValue::from_str(value.as_ref()).map_err(|_| invalid())?;
        if value.to_str().is_err() {
            return Err(invalid());
        }
        value.set_sensitive(true);
        self.headers.insert(header, value);
        Ok(self)
    }

    /// A request to the server with the caller's headers; every request path
    /// goes through here so none of them misses them.
    fn request(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
        self.client
            .request(method, &self.base_url)
            .headers(self.headers.clone())
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
    fn note_skipped(
        payload: &str,
        method: &str,
        skipped: &mut BTreeMap<String, usize>,
        notifications: Option<&ServerNotifications>,
    ) {
        let what = match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(value) => match value.get("method").and_then(|m| m.as_str()) {
                Some(m) => {
                    note_notification(m, notifications);
                    m.to_string()
                }
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
        notifications: Option<&ServerNotifications>,
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
                Self::note_skipped(&payload, method, &mut skipped, notifications);
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
                Self::note_skipped(&payload, method, &mut skipped, notifications);
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
            .request(reqwest::Method::POST)
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

        Self::read_response(
            resp,
            request_id,
            &method,
            status,
            self.notifications.as_ref(),
        )
        .await
    }

    async fn notify(&self, notification: JsonRpcNotification) -> Result<(), McpError> {
        let method = notification.method.clone();
        let mut builder = self
            .request(reqwest::Method::POST)
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

    fn report_notifications(&mut self, to: ServerNotifications) -> bool {
        self.notifications = Some(to);
        true
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
                .request(reqwest::Method::DELETE)
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

    /// A transport dropped while its process is locked (an exit-watch poll
    /// on another thread) gives up the group kill after its bounded retries:
    /// no panic, no hang.
    #[cfg(all(feature = "native", unix))]
    #[tokio::test]
    async fn a_drop_with_the_child_locked_does_not_hang() {
        let transport = StdioTransport::with_options(
            "sleep",
            &["30"],
            None,
            StdioOptions::new().own_process_group(true),
        )
        .await
        .unwrap();
        let child = transport.child.clone();
        let held = child.try_lock().unwrap();
        let started = std::time::Instant::now();
        drop(transport);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        drop(held);
    }

    /// Before the answer is latched (stderr still held open by a process the
    /// server left behind), a busy child gives the last answer unchanged.
    #[cfg(all(feature = "native", unix))]
    #[tokio::test]
    async fn a_busy_child_repeats_the_provisional_answer() {
        let transport =
            StdioTransport::new("bash", &["-c", "echo bye >&2; sleep 3 & exit 5"], None)
                .await
                .unwrap();
        let watch = transport.exit_watch().unwrap();
        // Until the answer carries the tail (the exit can be seen before the
        // drain has read it): the provisional answer, not yet latched.
        let mut first = None;
        for _ in 0..200 {
            first = watch.exited().filter(|why| why.contains("bye"));
            if first.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let first = first.expect("the exit was seen, with its stderr");
        let busy = transport.child.lock().await;
        assert_eq!(watch.exited().as_deref(), Some(first.as_str()));
        assert_eq!(watch.exited().as_deref(), Some(first.as_str()));
        drop(busy);
        assert_eq!(first.matches("last stderr").count(), 1, "{first}");
    }

    /// The watch never waits on the child's lock: busy (a call reaping the
    /// server, or `close`) reads as not known yet, then the exit is seen —
    /// with the server's last stderr.
    #[cfg(all(feature = "native", unix))]
    #[tokio::test]
    async fn the_exit_watch_does_not_wait_on_a_busy_child() {
        let transport = StdioTransport::new("bash", &["-c", "echo bye >&2; exit 5"], None)
            .await
            .unwrap();
        let watch = transport.exit_watch().unwrap();
        let busy = transport.child.lock().await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(watch.exited(), None, "the child's lock is held");
        drop(busy);
        let mut why = None;
        for _ in 0..200 {
            why = watch.exited();
            if why.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let why = why.expect("the exit was seen");
        assert!(
            why.contains("exit status: 5") && why.contains("bye"),
            "{why}"
        );
        assert_eq!(watch.exited().as_deref(), Some(why.as_str()), "latched");
    }

    #[test]
    fn caller_header_values_are_sensitive() {
        let transport = HttpTransport::new("http://127.0.0.1:9/mcp")
            .unwrap()
            .with_header("x-api-key", "s3cret")
            .unwrap();
        assert!(transport.headers["x-api-key"].is_sensitive());
        assert!(!format!("{:?}", transport.headers).contains("s3cret"));
    }

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
