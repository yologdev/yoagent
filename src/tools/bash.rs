//! Bash tool — execute shell commands with timeout and output capture.
//!
//! # This is not a sandbox
//!
//! `BashTool` runs whatever the model asks through `bash -c`, with the
//! process's own environment and filesystem access. [`deny_patterns`] is a
//! substring check that catches *typos and obvious mistakes* — it is trivially
//! bypassed by any command that means to (`rm  -rf /` with two spaces, a
//! base64-decoded pipe, an equivalent `find -delete`). Treat it as a guardrail,
//! never as a security boundary.
//!
//! Real isolation belongs outside this tool: run the agent in a container or
//! VM, or gate every call through
//! [`ToolMiddleware`], which sees the arguments
//! before execution and can deny them. For credentials specifically, see
//! [`BashTool::env_allowlist`].
//!
//! [`deny_patterns`]: BashTool::deny_patterns

use crate::types::*;

/// Type alias for command confirmation callback.
pub type ConfirmFn = Box<dyn Fn(&str) -> bool + Send + Sync>;
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;

/// How long the output is still read after `bash` exits, for what is
/// buffered in the pipes: past it, a background job that holds them is left
/// running, its later output discarded, and the result says so.
const READ_GRACE: Duration = Duration::from_secs(1);

/// Execute shell commands. Captures stdout + stderr.
pub struct BashTool {
    /// Working directory for commands
    pub cwd: Option<String>,
    /// Max execution time per command
    pub timeout: Duration,
    /// Max bytes captured from each of stdout and stderr. Reading stops
    /// keeping bytes past this (the rest is drained and discarded), so memory
    /// stays bounded however much a command prints.
    pub max_output_bytes: usize,
    /// Substrings that block a command outright.
    ///
    /// A convenience guardrail against typos, **not** a security control: a
    /// substring match is bypassed by whitespace, quoting, or encoding. See
    /// the module docs.
    pub deny_patterns: Vec<String>,
    /// When set, the child process receives only these environment variables
    /// (plus `PATH`, `HOME`, `PWD` if present).
    ///
    /// Commands otherwise inherit the agent's whole environment, including
    /// any `*_API_KEY` the process holds — so a model-authored command can
    /// read them. An allowlist is the cheap mitigation when running commands
    /// the model composed.
    pub env_allowlist: Option<Vec<String>>,
    /// Optional callback for confirming dangerous commands
    pub confirm_fn: Option<ConfirmFn>,
    /// Keep ANSI escape sequences (colours, cursor movement, terminal
    /// titles, hyperlinks) in the output. Off by default: they are stripped
    /// before the output reaches the model, which sees them only as noise.
    /// See [`with_raw_output`](Self::with_raw_output).
    pub raw_output: bool,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(120),
            max_output_bytes: 256 * 1024, // 256KB
            deny_patterns: vec![
                "rm -rf /".into(),
                "rm -rf /*".into(),
                "mkfs".into(),
                "dd if=".into(),
                ":(){:|:&};:".into(), // fork bomb
            ],
            confirm_fn: None,
            env_allowlist: None,
            raw_output: false,
        }
    }
}

impl BashTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Pass only these environment variables to commands (plus `PATH`,
    /// `HOME`, `PWD`). Use when the model composes the command.
    pub fn with_env_allowlist(mut self, vars: Vec<String>) -> Self {
        self.env_allowlist = Some(vars);
        self
    }

    pub fn with_deny_patterns(mut self, patterns: Vec<String>) -> Self {
        self.deny_patterns = patterns;
        self
    }

    pub fn with_confirm(mut self, f: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.confirm_fn = Some(Box::new(f));
        self
    }

    /// `true` keeps ANSI escape sequences in the output exactly as the
    /// command printed them. By default they are stripped: CSI sequences
    /// (colours, cursor movement), OSC strings (terminal titles,
    /// hyperlinks — the link text stays), the other string sequences (DCS,
    /// SOS, PM, APC) and two-character escapes. Carriage returns, tabs and
    /// backspaces are text and are always kept.
    pub fn with_raw_output(mut self, raw: bool) -> Self {
        self.raw_output = raw;
        self
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn label(&self) -> &str {
        "Execute Command"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return stdout/stderr. Use for running scripts, installing packages, checking system state, etc."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let cancel = ctx.cancel;
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'command' parameter".into()))?;

        // Guardrail only — see the module docs; this is not a security boundary.
        for pattern in &self.deny_patterns {
            if command.contains(pattern.as_str()) {
                return Err(ToolError::Failed(format!(
                    "Command blocked by safety policy: contains '{}'. This pattern is denied for safety.",
                    pattern
                )));
            }
        }

        // Check confirmation callback
        if let Some(ref confirm) = self.confirm_fn {
            if !confirm(command) {
                return Err(ToolError::Failed(
                    "Command was not confirmed by the user.".into(),
                ));
            }
        }

        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cmd = std::process::Command::new("bash");
        // std's `process_group` (Rust 1.64): tokio's own needs tokio 1.40,
        // newer than the `tokio = "1"` this crate asks for.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut cmd = Command::from(cmd);
        cmd.arg("-c").arg(command);

        // Keep credentials out of model-authored commands when configured.
        if let Some(ref allow) = self.env_allowlist {
            let keep: Vec<(String, String)> = std::env::vars()
                .filter(|(k, _)| {
                    allow.iter().any(|a| a == k) || matches!(k.as_str(), "PATH" | "HOME" | "PWD")
                })
                .collect();
            cmd.env_clear();
            for (k, v) in keep {
                cmd.env(k, v);
            }
        }

        if let Some(ref cwd) = self.cwd {
            cmd.current_dir(cwd);
        }

        // No stdin: a command that reads input gets EOF at once instead of
        // the agent's terminal (`spawn` inherits stdin; `output` did not).
        cmd.stdin(std::process::Stdio::null());
        // Capture output
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // A timeout or cancel must not leave the command running. On Unix
        // `bash` leads its own process group and `GroupKill` kills the whole
        // group: non-interactive bash has no job control, so pipeline stages,
        // `;` / `&&` lists and background jobs stay in it. Only a process that
        // starts its own session (`setsid`) escapes. Elsewhere this kills the
        // `bash` process only.
        cmd.kill_on_drop(true);

        let timeout = self.timeout;
        let max_bytes = self.max_output_bytes;
        let strip = !self.raw_output;

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Failed(format!("Failed to execute: {}", e)))?;
        // Declared after `child`, so it is dropped first: the group is killed
        // while `bash` is not yet reaped, and its id cannot have been reused.
        let mut group = GroupKill::new(child.id());
        let (Some(mut child_out), Some(mut child_err)) = (child.stdout.take(), child.stderr.take())
        else {
            return Err(ToolError::Failed("Failed to capture output".into()));
        };

        // The captures live outside the race, so a timeout still has what the
        // command printed before it, and whether it was cut.
        let mut out = Capture::default();
        let mut err = Capture::default();
        // Set once `bash` has exited, so a timeout or cancel that fires during
        // the read grace still sees a finished command (a cancel is still
        // reported, but leaves the command's jobs running).
        let mut exited = None;
        let outcome = {
            // Wait for `bash` itself, not for the pipes: a background job
            // inherits them, and one still running would keep them open until
            // the timeout. Once `bash` exits, what is buffered is read for
            // `READ_GRACE`; whether both pipes reached their end is returned.
            let run = async {
                let reads = async {
                    tokio::join!(
                        out.read(&mut child_out, max_bytes),
                        err.read(&mut child_err, max_bytes),
                    );
                };
                tokio::pin!(reads);
                let mut drained = false;
                exited = Some(tokio::select! {
                    () = &mut reads => {
                        drained = true;
                        child.wait().await
                    }
                    status = child.wait() => status,
                });
                drained || tokio::time::timeout(READ_GRACE, &mut reads).await.is_ok()
            };
            tokio::select! {
                _ = cancel.cancelled() => None,
                _ = tokio::time::sleep(timeout) => Some(None),
                drained = run => Some(Some(drained)),
            }
        };
        let cancelled = outcome.is_none();
        let drained = match (outcome, &exited) {
            (None, None) => return Err(ToolError::Cancelled),
            // The timeout fired before `bash` exited.
            (Some(None), None) => {
                let _ = child.start_kill();
                return Err(ToolError::Failed(format!(
                    "Command timed out after {}s. Output so far:\n{}",
                    timeout.as_secs(),
                    render_output(&out, &err, strip)
                )));
            }
            (Some(Some(drained)), _) => drained,
            // A timeout or cancel during the read grace: `bash` had finished.
            (_, Some(_)) => false,
        };
        let status = exited
            .expect("`bash` has exited")
            .map_err(|e| ToolError::Failed(format!("Failed to execute: {}", e)))?;
        // Finished on its own: what it left running in the background (a
        // server it started, say) is the command's business. (A cancel
        // during the grace still leaves it: `bash` is reaped, so its group id
        // is no longer ours to signal.)
        group.disarm();
        if !drained {
            // Keep reading what such a job prints, and discard it: closing
            // the pipes under it would kill it with SIGPIPE at its next write.
            // The task ends when the job closes them (it exits).
            tokio::spawn(async move {
                let (mut sink_out, mut sink_err) = (tokio::io::sink(), tokio::io::sink());
                let _ = tokio::join!(
                    tokio::io::copy(&mut child_out, &mut sink_out),
                    tokio::io::copy(&mut child_err, &mut sink_err),
                );
            });
        }
        if cancelled {
            return Err(ToolError::Cancelled);
        }
        let exit_code = status.code().unwrap_or(-1);
        let signal = exit_signal(&status);
        let header = match signal {
            Some(sig) => format!("Killed by signal {}", describe_signal(sig)),
            None => format!("Exit code: {exit_code}"),
        };
        let mut output = format!("{header}\n{}", render_output(&out, &err, strip));
        if !drained {
            output.push_str(
                "\n\nNOTE: a background process still holds the command's output and was \
                 left running; what it prints from now on is discarded.",
            );
        }

        // Return output even on failure — LLMs need error output to self-correct
        Ok(ToolResult {
            content: vec![Content::Text { text: output }],
            // `exit_code` is -1 when there is no exit status (the command was
            // killed by a signal); `signal` then names it, and is null
            // otherwise (always null off Unix).
            details: serde_json::json!({
                "exit_code": exit_code,
                "signal": signal,
                "success": exit_code == 0,
                "output_still_open": !drained,
            }),
        })
    }
}

/// The signal that terminated the `bash` process, if one did (Unix only).
/// That is the shell itself, or the command bash ran in its place (the last
/// simple command, or an `exec`ed one); a command killed earlier in a
/// compound script makes bash exit with `128 + n`, an ordinary exit code.
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        std::os::unix::process::ExitStatusExt::signal(status)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// `9 (SIGKILL)`, or just the number for a signal without a name here.
fn describe_signal(sig: i32) -> String {
    match signal_name(sig) {
        Some(name) => format!("{sig} ({name})"),
        None => sig.to_string(),
    }
}

/// The name of a common signal. Numbers differ between platforms (SIGBUS is
/// 7 on Linux and 10 on macOS), so they come from `libc`, never literals.
#[cfg(unix)]
fn signal_name(sig: i32) -> Option<&'static str> {
    const NAMES: &[(libc::c_int, &str)] = &[
        (libc::SIGHUP, "SIGHUP"),
        (libc::SIGINT, "SIGINT"),
        (libc::SIGQUIT, "SIGQUIT"),
        (libc::SIGILL, "SIGILL"),
        (libc::SIGTRAP, "SIGTRAP"),
        (libc::SIGABRT, "SIGABRT"),
        (libc::SIGBUS, "SIGBUS"),
        (libc::SIGFPE, "SIGFPE"),
        (libc::SIGKILL, "SIGKILL"),
        (libc::SIGUSR1, "SIGUSR1"),
        (libc::SIGSEGV, "SIGSEGV"),
        (libc::SIGUSR2, "SIGUSR2"),
        (libc::SIGPIPE, "SIGPIPE"),
        (libc::SIGALRM, "SIGALRM"),
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGXCPU, "SIGXCPU"),
        (libc::SIGXFSZ, "SIGXFSZ"),
        (libc::SIGSYS, "SIGSYS"),
    ];
    NAMES.iter().find(|(n, _)| *n == sig).map(|(_, name)| *name)
}

#[cfg(not(unix))]
fn signal_name(_: i32) -> Option<&'static str> {
    None
}

/// Kills the command's process group when dropped — on a timeout, a cancel,
/// or the tool's future being dropped — unless the command finished first.
struct GroupKill {
    #[cfg(unix)]
    pgid: Option<libc::pid_t>,
}

impl GroupKill {
    fn new(pid: Option<u32>) -> Self {
        #[cfg(not(unix))]
        let _ = pid;
        Self {
            #[cfg(unix)]
            pgid: pid.and_then(|p| libc::pid_t::try_from(p).ok()),
        }
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: `killpg` takes two integers and touches no memory of
            // ours; a failure (the group already gone) is fine to ignore.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

/// One output stream: the bytes kept, whether more were discarded, and a
/// read error if the stream ended on one.
#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    cut: bool,
    read_error: Option<String>,
}

impl Capture {
    /// Read `from` to the end, keeping at most `cap` bytes; the rest is
    /// drained and discarded so the child never blocks on a full pipe.
    async fn read<R>(&mut self, mut from: R, cap: usize)
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;
        let mut chunk = [0u8; 8192];
        loop {
            match from.read(&mut chunk).await {
                Ok(0) => return,
                Err(e) => {
                    self.read_error = Some(e.to_string());
                    return;
                }
                Ok(n) => {
                    let room = cap.saturating_sub(self.bytes.len());
                    self.bytes.extend_from_slice(&chunk[..n.min(room)]);
                    self.cut |= n > room;
                }
            }
        }
    }

    /// The text, with a note when bytes were discarded or reading failed.
    /// Bytes are cut before decoding, so a multi-byte character split by the
    /// cap becomes U+FFFD, never a panic. With `strip`, ANSI escapes are
    /// removed after decoding (they are ASCII, so the lossy decode never
    /// alters one) and before the notes are added; a sequence the cap cut
    /// short is dropped like a whole one.
    fn text(&self, strip: bool) -> String {
        let decoded = String::from_utf8_lossy(&self.bytes);
        let mut s = if strip {
            strip_ansi(&decoded)
        } else {
            decoded.into_owned()
        };
        if self.cut {
            s.push_str("\n... (output truncated)");
        }
        if let Some(e) = &self.read_error {
            s.push_str(&format!("\n... (output incomplete: read error: {e})"));
        }
        s
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty() && !self.cut && self.read_error.is_none()
    }
}

/// Stdout alone, or both streams labelled.
fn render_output(out: &Capture, err: &Capture, strip: bool) -> String {
    if err.is_empty() {
        out.text(strip)
    } else {
        format!("STDOUT:\n{}\nSTDERR:\n{}", out.text(strip), err.text(strip))
    }
}

const ESC: char = '\u{1b}';
/// The 8-bit CSI, as a decoded character (UTF-8 `C2 9B`).
const C1_CSI: char = '\u{9b}';
#[cfg(test)]
const BEL: char = '\u{7}';

/// The most bytes between an OSC, SOS, PM or APC introducer and its
/// terminator: a title or hyperlink is short, and a stray introducer must
/// not swallow the output after it up to some later BEL.
const STRING_MAX_BYTES: usize = 4 * 1024;

/// The same bound for DCS, which carries graphics (sixel) and so runs long.
const DCS_MAX_BYTES: usize = 64 * 1024;

/// `text` without ANSI escape sequences (ECMA-48):
///
/// - CSI: `ESC [` (or the 8-bit U+009B), parameter bytes `0x30–0x3F`,
///   intermediate bytes `0x20–0x2F`, a final byte `0x40–0x7E`. A sequence
///   broken by any other character ends there: what was read of it is
///   dropped, the character is kept.
/// - String sequences: `ESC ]` (OSC), `ESC P` (DCS), `ESC X` (SOS),
///   `ESC ^` (PM), `ESC _` (APC), up to and including the first BEL, ST
///   (`ESC \` or the 8-bit U+009C), or CAN / SUB (`0x18` / `0x1A`, which
///   abort a sequence), when that comes within [`STRING_MAX_BYTES`] bytes
///   ([`DCS_MAX_BYTES`] for DCS). An unterminated sequence — none within
///   the bound — loses only its two-character introducer, so a stray
///   `ESC ]` cannot hide the output after it.
/// - `ESC` + intermediates `0x20–0x2F` + a final `0x30–0x7E` (charset
///   selection such as `ESC ( B`), and `ESC` + any other character in
///   `0x30–0x7E` (`ESC 7`, `ESC M`, …).
/// - A lone `ESC` (the last character, or one before any other character,
///   which is kept).
///
/// Everything else, other control characters included, is kept.
fn strip_ansi(text: &str) -> String {
    strip_ansi_bounded(text, STRING_MAX_BYTES, DCS_MAX_BYTES)
}

/// [`strip_ansi`] with its bounds as parameters (tests use small ones).
///
/// Linear in the length of `text`: the end of the next string sequence is
/// looked for from where it is needed and remembered, and a later
/// introducer before that end reuses it, so no byte is searched twice.
/// Rescanning from every unterminated introducer was quadratic: a 256 KB
/// run of `ESC ]` took seconds.
fn strip_ansi_bounded(text: &str, string_max: usize, dcs_max: usize) -> String {
    if !text.contains([ESC, C1_CSI]) {
        return text.to_owned();
    }
    let mut ends = SequenceEnds::new(text);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == C1_CSI {
            skip_csi(&mut chars);
            continue;
        }
        if c != ESC {
            out.push(c);
            continue;
        }
        match chars.peek().map(|&(_, d)| d) {
            Some('[') => {
                chars.next();
                skip_csi(&mut chars);
            }
            Some(kind @ (']' | 'P' | 'X' | '^' | '_')) => {
                chars.next();
                let at = chars.peek().map_or(text.len(), |&(i, _)| i);
                let max = if kind == 'P' { dcs_max } else { string_max };
                // Within the bound, the sequence goes up to and including
                // its end; past it (or with none), only the introducer.
                if let Some((start, after)) = ends.next_from(at) {
                    if start - at <= max {
                        while chars.next_if(|&(i, _)| i < after).is_some() {}
                    }
                }
            }
            Some(' '..='/') => {
                while chars.next_if(|&(_, d)| (' '..='/').contains(&d)).is_some() {}
                chars.next_if(|&(_, d)| ('0'..='~').contains(&d));
            }
            Some('0'..='~') => {
                chars.next();
            }
            // A lone ESC: dropped, the next character kept.
            _ => {}
        }
    }
    out
}

/// Finds where string sequences end: the next BEL, `ESC \`, U+009C, CAN or
/// SUB at or after a position. Positions asked for only grow, so the last
/// answer is kept and reused while it still lies ahead.
struct SequenceEnds<'a> {
    bytes: &'a [u8],
    /// The last search: from where, and the end it found (start of the
    /// terminator, byte after it), `None` when there was none.
    last: Option<(usize, Option<(usize, usize)>)>,
}

impl<'a> SequenceEnds<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            last: None,
        }
    }

    fn next_from(&mut self, at: usize) -> Option<(usize, usize)> {
        if let Some((from, found)) = self.last {
            // Still valid: nothing between `from` and the found end (or the
            // end of the text) is a terminator.
            if from <= at && found.is_none_or(|(start, _)| start >= at) {
                return found;
            }
        }
        let found = self.search(at);
        self.last = Some((at, found));
        found
    }

    /// The first terminator at or after `at`. The bytes looked for are
    /// ASCII or (U+009C) the lead byte `C2`, which never occur inside
    /// another character's UTF-8, so a byte search is exact.
    fn search(&self, at: usize) -> Option<(usize, usize)> {
        let b = self.bytes;
        let mut i = at;
        while i < b.len() {
            match b[i] {
                0x07 | 0x18 | 0x1a => return Some((i, i + 1)),
                0x1b if b.get(i + 1) == Some(&b'\\') => return Some((i, i + 2)),
                0xc2 if b.get(i + 1) == Some(&0x9c) => return Some((i, i + 2)),
                _ => i += 1,
            }
        }
        None
    }
}

/// Skip a CSI sequence's parameters, intermediates and final byte, after
/// its introducer.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) {
    while chars.next_if(|&(_, d)| ('0'..='?').contains(&d)).is_some() {}
    while chars.next_if(|&(_, d)| (' '..='/').contains(&d)).is_some() {}
    chars.next_if(|&(_, d)| ('@'..='~').contains(&d));
}

#[cfg(test)]
mod tests {
    use super::{strip_ansi, strip_ansi_bounded};

    #[test]
    fn strips_sgr_colours_and_cursor_movement() {
        assert_eq!(strip_ansi("\x1b[1;31merror\x1b[0m: bad"), "error: bad");
        assert_eq!(strip_ansi("a\x1b[2Kb\x1b[10;20Hc\x1b[?25ld"), "abcd");
        assert_eq!(strip_ansi("\u{9b}31mred"), "red");
    }

    #[test]
    fn strips_osc_titles_and_keeps_hyperlink_text() {
        assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
        assert_eq!(
            strip_ansi("\x1b]8;;https://x.test\x1b\\link\x1b]8;;\x1b\\ done"),
            "link done"
        );
        assert_eq!(strip_ansi("\x1bPq#0\x1b\\x"), "x");
    }

    #[test]
    fn the_8_bit_st_terminates_and_can_or_sub_abort() {
        assert_eq!(strip_ansi("\x1b]0;title\u{9c}after"), "after");
        assert_eq!(strip_ansi("\x1b]0;tit\x18after"), "after");
        assert_eq!(strip_ansi("\x1bPq#0\x1aafter"), "after");
        // Outside a sequence they are text.
        assert_eq!(strip_ansi("a\u{9c}\x18\x1ab"), "a\u{9c}\x18\x1ab");
    }

    /// A stray introducer, 10 KB of output, then a BEL: the output stays.
    #[test]
    fn a_terminator_past_the_bound_does_not_swallow_the_output() {
        let text = "real output line\n".repeat(600);
        let input = format!("\x1b]{text}\x07end");
        assert_eq!(strip_ansi(&input), format!("{text}\x07end"));
        // A DCS has a larger bound: 10 KB of sixel data is removed.
        let sixel = "#0;2;0;0;0~~@@vv@@~~$".repeat(500);
        assert_eq!(strip_ansi(&format!("\x1bPq{sixel}\x1b\\end")), "end");
        // Past its own bound, a DCS also loses only its introducer.
        let long = "x".repeat(super::DCS_MAX_BYTES + 1);
        assert_eq!(
            strip_ansi(&format!("\x1bP{long}\x07")),
            format!("{long}\x07")
        );
    }

    #[test]
    fn strips_two_character_and_charset_escapes() {
        assert_eq!(strip_ansi("a\x1b7b\x1b8c\x1bMd"), "abcd");
        assert_eq!(strip_ansi("\x1b(Bplain\x1b)0"), "plain");
    }

    #[test]
    fn keeps_text_and_other_control_characters() {
        let plain = "line\r\n\ttab\x08 日本語 [31m";
        assert_eq!(strip_ansi(plain), plain);
    }

    #[test]
    fn an_unterminated_string_sequence_loses_only_its_introducer() {
        assert_eq!(
            strip_ansi("\x1b]0;never closed\nmore"),
            "0;never closed\nmore"
        );
    }

    #[test]
    fn a_sequence_cut_short_at_the_end_is_dropped() {
        assert_eq!(strip_ansi("ok\x1b[1;3"), "ok");
        assert_eq!(strip_ansi("ok\x1b["), "ok");
        assert_eq!(strip_ansi("ok\x1b"), "ok");
    }

    #[test]
    fn a_broken_csi_keeps_the_character_that_broke_it() {
        assert_eq!(strip_ansi("\x1b[31\n日"), "\n日");
        assert_eq!(strip_ansi("\x1b\u{e9}x"), "\u{e9}x");
    }

    /// 256 KB (the default `max_output_bytes`) of string introducers that
    /// never close. Rescanning to the end from each one took ~15 s; linear
    /// takes milliseconds, so the ceiling is generous and still catches it.
    #[test]
    fn unterminated_string_sequences_take_linear_time() {
        let n = 256 * 1024 / 2;
        let ceiling = if cfg!(debug_assertions) {
            std::time::Duration::from_secs(5)
        } else {
            std::time::Duration::from_secs(1)
        };
        for (input, expected) in [
            ("\x1b]".repeat(n), String::new()),
            ("\x1bP".repeat(n), String::new()),
            // A terminator before them all does not help them.
            (
                format!("a\x07{}", "\x1b_x".repeat(n / 2)),
                "a\x07".to_owned() + &"x".repeat(n / 2),
            ),
            // One at the very end: the introducers before it lose
            // themselves, the last few (within the bound) reach it.
            (format!("{}\x07b", "\x1b]".repeat(n)), "b".to_owned()),
        ] {
            let started = std::time::Instant::now();
            let out = strip_ansi(&input);
            let took = started.elapsed();
            assert_eq!(out, expected);
            assert!(took < ceiling, "{} bytes took {took:?}", input.len());
        }
    }

    /// The definition, written the obvious (quadratic) way: from each string
    /// introducer, look ahead char by char for the first BEL, `ESC \`,
    /// U+009C, CAN or SUB within the bound.
    fn reference(text: &str, string_max: usize, dcs_max: usize) -> String {
        let mut out = String::new();
        let mut chars = text.char_indices().peekable();
        while let Some((_, c)) = chars.next() {
            if c == super::C1_CSI {
                skip(&mut chars);
                continue;
            }
            if c != super::ESC {
                out.push(c);
                continue;
            }
            match chars.peek().map(|&(_, d)| d) {
                Some('[') => {
                    chars.next();
                    skip(&mut chars);
                }
                Some(kind @ (']' | 'P' | 'X' | '^' | '_')) => {
                    chars.next();
                    let at = chars.peek().map_or(text.len(), |&(i, _)| i);
                    let max = if kind == 'P' { dcs_max } else { string_max };
                    let mut ahead = chars.clone();
                    while let Some((i, d)) = ahead.next() {
                        if i - at > max {
                            break;
                        }
                        let ends = matches!(d, super::BEL | '\u{9c}' | '\x18' | '\x1a')
                            || (d == super::ESC && ahead.next_if(|&(_, e)| e == '\\').is_some());
                        if ends {
                            chars = ahead;
                            break;
                        }
                    }
                }
                Some(' '..='/') => {
                    while chars.next_if(|&(_, d)| (' '..='/').contains(&d)).is_some() {}
                    chars.next_if(|&(_, d)| ('0'..='~').contains(&d));
                }
                Some('0'..='~') => {
                    chars.next();
                }
                _ => {}
            }
        }
        out
    }

    fn skip(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) {
        while chars.next_if(|&(_, d)| ('0'..='?').contains(&d)).is_some() {}
        while chars.next_if(|&(_, d)| (' '..='/').contains(&d)).is_some() {}
        chars.next_if(|&(_, d)| ('@'..='~').contains(&d));
    }

    /// Same output as the reference on 40,000 pseudo-random strings over the
    /// characters that matter, with small bounds so they are reached.
    #[test]
    fn linear_strip_matches_the_reference() {
        const ALPHABET: [char; 21] = [
            '\x1b', '\x1b', ']', 'P', '_', 'X', '\\', '\x07', '[', '1', ';', 'm', '(', ' ', '?',
            'a', 'é', '\u{9b}', '\u{9c}', '\x18', '\x1a',
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        for round in 0..40_000 {
            let len = next() % 32;
            let text: String = (0..len)
                .map(|_| ALPHABET[next() % ALPHABET.len()])
                .collect();
            let (string_max, dcs_max) = if round % 2 == 0 {
                (4, 9)
            } else {
                (4096, 65536)
            };
            assert_eq!(
                strip_ansi_bounded(&text, string_max, dcs_max),
                reference(&text, string_max, dcs_max),
                "{text:?} with bounds {string_max}/{dcs_max}"
            );
        }
    }
}
