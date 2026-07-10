# Session Trees

A `Session` stores conversation history as a **tree**, not a flat list. Every
entry has an `id` and `parent_id`; the *head* points at the current branch
tip. Appending after a `seek` creates a **new branch** — history is never
overwritten. This is the primitive behind:

- **Fork** — try a different direction from any point
- **Checkpoints** — label a state, come back to it later
- **Edit & re-run** — change an earlier turn on a new branch; the original
  branch stays intact

```rust
use yoagent::{Agent, Session, provider::ModelConfig};

let mut session = Session::new();

// Run a turn, record it.
let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"));
let mut rx = agent.prompt("draft a plan").await;
while rx.recv().await.is_some() {}
agent.finish().await;
session.append_new(agent.messages());
session.checkpoint("first-draft")?;

// ... more turns ...

// Rewind to the checkpoint and branch.
session.seek_checkpoint("first-draft")?;
let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"))
    .with_messages(session.path_messages());   // only this branch's history
let mut rx = agent.prompt("actually, make it a library instead").await;
while rx.recv().await.is_some() {}
agent.finish().await;
session.append_new(agent.messages());          // new branch recorded

// Both branches exist:
assert_eq!(session.branch_tips().len(), 2);
```

## Persistence: JSONL

```rust
std::fs::write("session.jsonl", session.to_jsonl())?;
let restored = Session::from_jsonl(&std::fs::read_to_string("session.jsonl")?)?;
```

One entry per line, append-friendly, diff-friendly. On load the head is the
last line's entry. The flat `save_messages()` / `restore_messages()` API
remains for single-branch persistence.

## API sketch

| Method | Purpose |
|---|---|
| `append(msg) -> id` | Add as child of head, advance head |
| `append_new(&agent_messages)` | Record everything beyond the current path — the post-run sync |
| `seek(id)` / `seek_checkpoint(label)` | Move the head (fork point) |
| `checkpoint(label)` | Label the head |
| `path_messages()` | Root→head messages — feed to `Agent::with_messages` |
| `branch_tips()` / `children(id)` / `entries()` | Inspect the tree |
| `to_jsonl()` / `from_jsonl()` | Persist / restore |

## GASP

This tree is the shape of the [GASP](https://github.com/yologdev/gasp)
`transcripts/` tier — the raw-conversation cold tier. The semantic event log
(goals/runs/evals living in a git repo) is a separate adapter layered on the
`AgentEvent` stream.
