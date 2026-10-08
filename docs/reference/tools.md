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
}
```

Default deny patterns: `rm -rf /`, `rm -rf /*`, `mkfs`, `dd if=`, fork bomb. These are substring
guardrails against typos, not a security control.

By default a command inherits the agent's whole environment, including any `*_API_KEY`. Set
`env_allowlist` to pass only the listed variables (plus `PATH`, `HOME`, `PWD` if present).

Output beyond `max_output_bytes` is drained and discarded, so memory stays bounded. A timeout
returns an error carrying the output so far. A timeout or cancel kills the `bash` process, but not
what it started (pipeline stages, `&&` lists, background jobs).

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
}
```

Every file tool (`read_file`, `write_file`, `edit_file`, `list_files`, `search`) has
`allowed_paths` and a `with_allowed_paths(..)` builder. The check runs against the *resolved*
path, so `..` and symlinks (dangling ones included) cannot escape, and the tool does its I/O on
the checked path (see `PathSandbox`). Before 0.24.2 a `..` after a missing directory, or a
dangling symlink, could get past the check.

## WriteFileTool

Write content to a file. Creates parent directories automatically.

- **Name**: `write_file`
- **Parameters**: `path` (required), `content` (required)
- **Configuration**: `allowed_paths: Vec<String>`

## EditFileTool

Surgical search/replace edits. The most important tool for coding agents — instead of rewriting entire files, the agent specifies exact text to find and replace.

- **Name**: `edit_file`
- **Parameters**: `path` (required), `old_text` (required), `new_text` (required)
- **Configuration**: `allowed_paths: Vec<String>`

The `old_text` must match exactly, including whitespace and indentation.

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
