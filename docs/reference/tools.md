# Built-in Tools

yoagent ships with six coding-oriented tools for native hosts (the default `native` feature; not available on wasm32). Get them all with `default_tools()`:

```rust
use yoagent::tools::default_tools;
let tools = default_tools();
```

## BashTool

Execute shell commands with timeout and output capture.

- **Name**: `bash`
- **Parameters**: `command` (string, required)

### Configuration

```rust
pub struct BashTool {
    pub cwd: Option<String>,           // Working directory
    pub timeout: Duration,             // Default: 120s
    pub max_output_bytes: usize,       // Default: 256KB, per stream (stdout and stderr each)
    pub deny_patterns: Vec<String>,    // Blocked commands
    pub env_allowlist: Option<Vec<String>>, // Default: None (inherit the whole environment)
    pub confirm_fn: Option<ConfirmFn>, // Confirmation callback
    pub raw_output: bool,              // Default: false (ANSI escapes stripped)
}
```

Default deny patterns: `rm -rf /`, `rm -rf /*`, `mkfs`, `dd if=`, fork bomb. These are substring
guardrails against typos, not a security control.

By default a command inherits the agent's whole environment, including any `*_API_KEY`. Set
`env_allowlist` to pass only the listed variables (plus `PATH`, `HOME`, `PWD` if present).

Past `max_output_bytes`, each stream keeps its first half and its last half, with
`... (output truncated: N bytes omitted) ...` between them; the rest is drained and discarded,
so memory stays bounded and a long log keeps its end (where a build or test verdict is). Before
0.27.5 only the start was kept. A timeout
returns an error carrying the output so far. On Unix a timeout or cancel kills the whole process
group (pipeline stages, `&&` lists, background jobs; only a process that starts its own session
escapes); elsewhere it kills the `bash` process only.

A command that finishes on its own leaves its background jobs running (a server it started, say),
even if the call is cancelled after `bash` exits. The call ends when `bash` exits: the output is
read for one more second, and a job that still holds the output pipes then is left running. What
it prints from then on is read and discarded in the background (so a closed pipe does not kill
it; a job that prints without pause keeps that reading busy), and the result says so with a
`NOTE:` line and `details.output_still_open: true`. Redirect a job's output (`cmd >log 2>&1 &`)
to keep it. Before 0.25.8 such a job held the call until the timeout, which then killed it.

The result starts with `Exit code: N`, or, when the `bash` process itself was killed by a
signal (Unix), with `Killed by signal 9 (SIGKILL)`. That happens when the shell is killed, or
when the signal hits the command's last simple command (bash `exec`s it, so it *is* the shell)
or an `exec`ed one. A command killed earlier in a compound script (`a; b`, a pipeline, a
subshell) is reported by bash itself as `Exit code: 128+n` (137 for SIGKILL), and the library
does not guess which. `details.exit_code` is the exit status, `-1` when there is none (a signal);
`details.signal` is the signal number, `null` otherwise (always `null` off Unix). Up to 0.26.8 a
killed shell reported only `Exit code: -1`.

ANSI escape sequences (colours, cursor movement, terminal titles, hyperlinks, whose text stays)
are stripped from both streams before the output reaches the model, the timeout error's output
included; `with_raw_output(true)` (or `raw_output: true`) keeps them. Carriage returns, tabs and
backspaces are kept either way. A string sequence (OSC, DCS, SOS, PM, APC) is removed up to its
BEL, ST (`ESC \` or the 8-bit U+009C) or CAN/SUB (which abort it), when that comes within 4 KiB
(64 KiB for DCS, which carries sixel graphics); an unterminated one (none within the bound)
loses only its introducer, so a stray `ESC ]` cannot hide the output after it. The cap counts
the bytes as printed, escapes included, and a sequence the cap cuts in half is dropped too.

### Example

```rust
let bash = BashTool::default();
// Or customize:
let bash = BashTool {
    cwd: Some("/workspace".into()),
    timeout: Duration::from_secs(60),
    ..Default::default()
};
```

## ReadFileTool

Read file contents with optional line range.

- **Name**: `read_file`
- **Parameters**: `path` (required), `offset` (optional, 1-indexed line), `limit` (optional, number of lines)

### Configuration

```rust
pub struct ReadFileTool {
    pub max_bytes: usize,              // Default: 1MB
    pub allowed_paths: Vec<String>,    // Path restrictions (empty = no restriction)
    pub max_lines: usize,              // Default: 500 when the call gives no limit (usize::MAX = unbounded)
    pub read_log: Option<ReadLog>,     // Default: None; shared with WriteFileTool (see below)
}
```

Every file tool (`read_file`, `write_file`, `edit_file`, `list_files`, `search`) has
`allowed_paths` and a `with_allowed_paths(..)` builder. The check runs against the *resolved*
path, so `..` and symlinks (dangling ones included) cannot escape, and the tool does its I/O on
the checked path (see `PathSandbox`). Before 0.24.2 a `..` after a missing directory, or a
dangling symlink, could get past the check.

A file over `max_bytes` is refused when the call gives no `offset`/`limit`; with them it is
read a page at a time, streamed in fixed chunks, so memory stays bounded: lines before the page
are only counted (the header gives the true total), a kept line is cut at `max_bytes`
(`… (line cut at N bytes)`), and the page stops once it holds `max_bytes`. Before 0.27.5 such a
file was refused even with `offset`/`limit`.

`read_file`, `write_file` and `edit_file` refuse a path that is not a regular file (after
following symlinks): a FIFO, a device, a socket or a directory. Before 0.25.8 a FIFO passed the
size check (its length reads 0) and the read blocked until something wrote to it, and a device
such as `/dev/zero` was read without end.

## WriteFileTool

Write content to a file. Creates parent directories automatically.

- **Name**: `write_file`
- **Parameters**: `path` (required), `content` (required)
- **Configuration**: `allowed_paths: Vec<String>`, `read_log: Option<ReadLog>`

`write_file` and `edit_file` replace a file whole: they write a temp file in the same directory,
flush it to disk and rename it over the target, so a crash, a full disk or a write error leaves
the old file intact and a reader never sees a half-written one (before 0.27.5 they wrote in
place, truncating first). What an in-place write did is kept: a file that cannot be written in
place (read-only, another user's) is refused, a symlink is followed (a dangling one is written
through, creating its target), the file's mode is kept — and on Unix its owner and group, best
effort — and a directory where no file can be created, or a sticky directory that refuses the
rename, falls back to writing in place. The rename gives the file a new inode: a hard link to the
old one keeps the old content, and ACLs and extended attributes are not carried over.

With a `ReadLog` shared with `read_file` (`with_read_log(log)` on both; `default_tools()` does
this), `write_file` refuses to overwrite an existing file that this agent has read only in part,
naming the lines it read and the file's length, so the model reads the rest or uses `edit_file`.
A full read of the current version (in one call or in pages; later pages of that version keep
it), the agent's own successful write, or never having read the file lets the write through; a
file changed on disk since the partial read stays refused until read again, and a read showing
no lines records nothing. Without a log (the field's default) it overwrites as before. See
[Tools: `write_file` after a partial read](../concepts/tools.md#write_file-after-a-partial-read).

## EditFileTool

Surgical search/replace edits. The most important tool for coding agents — instead of rewriting entire files, the agent specifies exact text to find and replace.

- **Name**: `edit_file`
- **Parameters**: `path` (required), `old_text` (required), `new_text` (required)
- **Configuration**: `allowed_paths: Vec<String>`

The `old_text` must match exactly, including whitespace and indentation. One exception: an
`old_text` written with `\n` line breaks (models cannot see a CRLF file's `\r`) that is not
found as given is looked up with each bare `\n` spelled `\r\n`; that spelling must match exactly
once too. An exact match always wins.

`new_text` is written with the line endings of the place it goes: where the matched text's line
breaks are all `\r\n` (or, for a match within one line, where that line ends in `\r\n`), each
bare `\n` of `new_text` becomes `\r\n`, so a CRLF file never gains bare `\n`. Anywhere else
`new_text` is written as given; an LF file is never touched. `details.crlf` is `true` when either
text was respelled.

When `old_text` is not found, the error shows where its first line occurs ("Did you mean"),
numbered like `read_file`'s output, each line cut to 400 characters with `… (line truncated)`,
so a minified file cannot blow up the error. Past 20 lines it shows 20 around the first line that
differs from `old_text`, and counts the lines left out above and below. Whitespace is never fixed
up for the model.

## ListFilesTool

List files recursively with optional glob filtering.

- **Name**: `list_files`
- **Parameters**: `path` (optional, default: `.`), `pattern` (optional glob), `max_depth` (optional, default: 3)

### Configuration

```rust
pub struct ListFilesTool {
    pub max_results: usize,    // Default: 200
    pub timeout: Duration,     // Default: 10s
    pub allowed_paths: Vec<String>,
}
```

Uses `find`, skipping `target/`, `.git/` and `node_modules/`. Paths `find` can't read (a subdirectory without permission) don't fail the listing: the files it did find are returned, with the errors under `Warnings` and in `details.warnings`, so a partial listing is never presented as complete. A listing with no files and an error is a failure.

## SearchTool

Search files using ripgrep, falling back to grep.

- **Name**: `search`
- **Parameters**: `pattern` (required, regex), `path` (optional), `include` (optional file glob, e.g. `*.rs`), `case_sensitive` (optional, default false)

### Configuration

```rust
pub struct SearchTool {
    pub root: Option<String>,      // Root directory
    pub max_results: usize,        // Default: 50, in total
    pub timeout: Duration,         // Default: 30s
    pub allowed_paths: Vec<String>,
}
```

Returns matching lines with file paths and line numbers. Past `max_results` the search is stopped
and the result says there are more (`details.truncated`). The pattern and path are passed so they
can never be read as flags. When some files cannot be searched (unreadable, removed mid-walk), the
matches found elsewhere are still returned, followed by the tool's stderr under `Warnings:`
(`details.warnings` holds the same text, capped at 2,000 characters, or `null`); only a search
that found nothing and hit an error fails.

The tool uses ripgrep when an `rg` is on `PATH` (looked up directly, no `which` needed) and
`grep -r` otherwise; `details.engine` says which ran. Your own options never reach it: `rg` runs
with `--no-config`, and `RIPGREP_CONFIG_PATH` and `GREP_OPTIONS` are cleared, so neither can change
the `path:line:text` output or follow a symlink out of `allowed_paths`. They still apply to `rg` or
`grep` run through `bash`. A search that ends abnormally (killed by a signal, an unexpected exit
code, an unreadable output) is an error when it found nothing, and its matches carry a warning
otherwise: it never reads as "No matches".

## SharedStateTool

Read and write named variables in a shared key-value store. This tool is **not** included in `default_tools()` — it is automatically injected into sub-agents when you call `SubAgentTool::with_shared_state()`, or into the agent itself with `Agent::with_shared_state()`. Unlike the tools above it also works on wasm32.

- **Name**: `shared_state`
- **Parameters**: `action` (required: `get`, `set`, `list`, `remove`), `key` (required for get/set/remove), `value` (required for set)

| Action | Description |
|--------|-------------|
| `get` | Returns the value for a key, or error if not found |
| `set` | Stores a value, returns confirmation with byte size |
| `list` | Lists all keys with their byte sizes |
| `remove` | Deletes a key |

See [Sub-Agents: Shared State](../concepts/sub-agents.md#shared-state) for usage details.
