# Built-in Tools

yoagent ships with six coding-oriented tools. Get them all with `default_tools()`:

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
    pub max_output_bytes: usize,       // Default: 256KB
    pub deny_patterns: Vec<String>,    // Blocked commands
    pub confirm_fn: Option<ConfirmFn>, // Confirmation callback
}
```

Default deny patterns: `rm -rf /`, `rm -rf /*`, `mkfs`, `dd if=`, fork bomb.

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
}
```

## WriteFileTool

Write content to a file. Creates parent directories automatically.

- **Name**: `write_file`
- **Parameters**: `path` (required), `content` (required)

## EditFileTool

Surgical search/replace edits. The most important tool for coding agents — instead of rewriting entire files, the agent specifies exact text to find and replace.

- **Name**: `edit_file`
- **Parameters**: `path` (required), `old_text` (required), `new_text` (required)

The `old_text` must match exactly, including whitespace and indentation.

## ListFilesTool

List files and directories with optional glob filtering.

- **Name**: `list_files`
- **Parameters**: `path` (optional, default: `.`), `pattern` (optional glob)

### Configuration

```rust
pub struct ListFilesTool {
    pub max_results: usize,    // Default: 200
    pub timeout: Duration,     // Default: 10s
}
```

Uses `find` or `fd` for efficient traversal.

## SearchTool

Search files using grep (or ripgrep if available).

- **Name**: `search`
- **Parameters**: `pattern` (required, regex), `path` (optional root directory)

### Configuration

```rust
pub struct SearchTool {
    pub root: Option<String>,      // Root directory
    pub max_results: usize,        // Default: 50
    pub timeout: Duration,         // Default: 30s
}
```

Returns matching lines with file paths and line numbers.

## SharedStateTool

Read and write named variables in a shared key-value store. This tool is **not** included in `default_tools()` — it is automatically injected into sub-agents when you call `SubAgentTool::with_shared_state()`.

- **Name**: `shared_state`
- **Parameters**: `action` (required: `get`, `set`, `list`, `remove`), `key` (required for get/set/remove), `value` (required for set)

| Action | Description |
|--------|-------------|
| `get` | Returns the value for a key, or error if not found |
| `set` | Stores a value, returns confirmation with byte size |
| `list` | Lists all keys with their byte sizes |
| `remove` | Deletes a key |

See [Sub-Agents: Shared State](../concepts/sub-agents.md#shared-state) for usage details.
