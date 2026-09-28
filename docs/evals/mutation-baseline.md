# Mutation testing: baseline

Issue #163. `cargo-mutants` injects small bugs, such as flipping `<` to `<=` or
replacing a function body with `Default::default()`, and reruns the tests. A
mutant the suite does not notice is a **missed** mutant. It is either an
assertion that cannot fail for the reason it states, or a mutation that changes
nothing observable. This page records the first measurement and sorts every
missed mutant into one of those two groups. Future runs should then be triaged
by what *changed*, without arguing again over the known survivors.

Mutation score is a means, not a target. The useful signal is narrower than
coverage: code that runs under test while nothing checks what it does.

## Setup

| | |
|---|---|
| Config | [`.cargo/mutants.toml`](https://github.com/yologdev/yoagent/blob/main/.cargo/mutants.toml) |
| CI | [`.github/workflows/mutants.yml`](https://github.com/yologdev/yoagent/blob/main/.github/workflows/mutants.yml): weekly (Mon 03:17 UTC) + manual, 8 round-robin shards |
| Scope | `src/context.rs`, `src/agent_loop.rs`, `src/llm_compaction.rs`, `src/provider/model.rs`, `src/provider/prices.rs`, `src/provider/prices/global.rs` |
| Build | `--all-features`, `--tests` (no doctests or examples), profile `mutants` (test profile, opt-level 1, no debuginfo) |
| Excluded | the empty-body mutants of the two redacting `Debug` impls (see the config for why) |

Pricing has been data since 0.20, so it lives in three files. `model.rs` holds
the arithmetic (`CostConfig::cost_usd`, context tiers) and the lookup made at
construction. `prices.rs` holds validation, layering and change reports.
`prices/global.rs` resolves the layers that every constructor reads through.
`prices/fetch.rs` is left out: it runs only when asked, and
`tests/price_audit.rs` diffs its mapping against models.dev.

Mutants in scope, from `cargo mutants --list` with cargo-mutants 27.1.0 at
9d060c7:

| module | mutants |
|---|---:|
| `src/context.rs` | 269 |
| `src/provider/model.rs` | 222 |
| `src/llm_compaction.rs` | 153 |
| `src/agent_loop.rs` | 138 |
| `src/provider/prices.rs` | 90 |
| `src/provider/prices/global.rs` | 56 |
| **total** | **928** |

## Baseline run — 2026-09-28

A full local run was estimated at about 14 hours, so the baseline is a
**sample**. It is round-robin shard 0 of 10 over all 928 mutants (mutant *i*
for every *i* ≡ 0 mod 10), which spreads the sample across every module in
proportion to its size:

```sh
# cargo-mutants 27.1.0, rustc 1.94.0, Apple M2 (8 cores), commit 9d060c7 + this config
cargo mutants -j 2 --shard 0/10 --sharding round-robin
```

Result: `93 mutants tested in 86m: 22 missed, 58 caught, 12 unviable, 1 timeouts`
(exit code 3). The unmutated baseline took 748 s to build from cold and 54 s to
test, so the timeout was set automatically to 165 s. The machine was heavily
loaded by other work during the run (load average 30–185), which makes the
wall time an upper bound.

| module | caught | missed | timeout | unviable | tested |
|---|---:|---:|---:|---:|---:|
| `src/agent_loop.rs` | 7 | 6 | 0 | 1 | 14 |
| `src/context.rs` | 20 | 6 | 1 | 0 | 27 |
| `src/llm_compaction.rs` | 9 | 4 | 0 | 2 | 15 |
| `src/provider/model.rs` | 15 | 5 | 0 | 3 | 23 |
| `src/provider/prices.rs` | 5 | 1 | 0 | 3 | 9 |
| `src/provider/prices/global.rs` | 2 | 0 | 0 | 3 | 5 |
| **total** | **58** | **22** | **1** | **12** | **93** |

Of the viable mutants, 59 of 81 were detected (58 caught, plus 1 timeout).
The timeout (`context.rs:751` `+=` → `*=`) is an infinite loop, which counts as
detected. The 12 unviable mutants do not compile, because the type has no
`Default`, and say nothing about the tests.

### Missed mutants — real test gaps (13)

Nothing is fixed here. These are listed so they can be fixed separately. Line
numbers are at 9d060c7.

| where | mutant | why no test notices |
|---|---|---|
| `agent_loop.rs:556` | `turn_number > 0` → `<` in `run_loop` (turn delay never sleeps) | No test sets `turn_delay: Some(..)` or `with_turn_delay`, so the throttle is never exercised. |
| `agent_loop.rs:623` | `ratio != compact_target_ratio` → `==` in `run_loop` (headroom-adapted ratio discarded) | `effective_target_ratio` is tested directly (`context_cache_test`), but no loop-level test checks that compaction *in the loop* uses the adapted ratio when `compact_headroom_turns` is set. |
| `agent_loop.rs:1157` | delete `!` in `!steering.is_empty()` (steering collected during tool execution dropped) | No test checks that steering messages returned by `execute_tool_calls` (queued mid-batch) reach the next turn's context. The mutant replaces them with a second `get_steering_messages()` poll, which has already been drained. |
| `agent_loop.rs:1311` | delete the `StreamEvent::Error` match arm in `stream_assistant_response` | No test streams an SSE-embedded error through the loop and checks for `MessageStart`/`MessageEnd` events. The GASP recorder's `model.finished` depends on that `MessageEnd`. |
| `context.rs:35` | `+ 4` → `- 4` for `AgentMessage::Extension` in `message_tokens` | No test estimates tokens for an extension message. An extension whose payload estimates to fewer than 4 tokens would underflow (a panic in debug). |
| `context.rs:728` | delete `!` in `!text_parts.is_empty()` in `level2_summarize_old_turns` | No test checks *what* a level-2 summary says. With the mutant, a turn with text becomes a placeholder (`[Assistant used N tool(s)]` or `[Assistant response]`), and a tool-only turn becomes an empty summary. |
| `context.rs:826` | `end -= 1` → `+= 1` in `safe_head_end` | No test puts the kept-head boundary directly after an assistant message that opens tool calls, which is the case this function exists for. |
| `context.rs:844` | `start -= 1` → `+= 1` in `safe_turn_start` | Same gap on the other side: no test lands a split on a tool result and checks that the whole turn stays together. |
| `context.rs:1028` | `signature_hash` → `0` | Loop-detection tests repeat one signature at a time. Nothing puts two *different* repeated signatures in one session, where a constant hash makes the second look already steered. |
| `llm_compaction.rs:1291` | `&&` → `\|\|` in `clip` (every over-long block clipped to `""`) | No test checks that a clipped block keeps its first `max` bytes. An empty result still satisfies any "at most `max`" check. |
| `provider/model.rs:217` | last `!=` → `==` in `ContextTier::is_configured` (an all-zero tier reports configured) | No test calls `is_configured` on an all-zero `ContextTier`, or on a `CostConfig` whose only tier is all-zero. |
| `provider/model.rs:1283` | delete `max_tokens` from `claude_fable_5` (64 000 → the `anthropic()` default 16 000) | `tests/preset_prices_test.rs` pins every preset's full `CostConfig`. Nothing pins the presets' `max_tokens`. |
| `provider/model.rs:1410` | delete `max_tokens` from `claude_sonnet_5` (same) | Same. Other presets that override `max_tokens` probably have the same gap (not checked); the sample happened to include these two. |

Two more are **boundary cases**. They are real but low priority:

| where | mutant | note |
|---|---|---|
| `llm_compaction.rs:780` | `<` → `<=` in `choose_cut` | Picks `HistoryTooShort` over `TailTooLarge` only at exact equality. The only effect is which one-shot warning fires. |
| `llm_compaction.rs:1170` | `>` → `>=` in `compact` | At exactly `budget` tokens the tail is compacted again. That is not a no-op, because Level 1 truncation runs anyway, but it only happens at a single token count. |

### Missed mutants — equivalent or acceptable (7)

Do not reopen these unless the code around them changes.

| where | mutant | why it cannot matter |
|---|---|---|
| `agent_loop.rs:591` | `growth_samples > 0` → `>=` | With zero samples the mutant computes `0/0 = NaN`. `effective_target_ratio` treats non-finite growth exactly like `0.0` ("no usable estimate yet"), so the result is the same. **Equivalent.** |
| `agent_loop.rs:1514` | `executed < len` → `<=` (batched skip) | At `executed == len` the slice `tool_calls[len..]` is empty, so the loop does nothing either way. **Equivalent.** |
| `context.rs:422` | `after_l1 < before` → `<=` | Guards only a `tracing::debug!` line. **Logging only.** |
| `llm_compaction.rs:1101` | `attempt + 1` → `attempt * 1` in the retry sleep | Shortens the backoff by one step for retries after the first. At attempt 0 the result is identical, because `delay_for_attempt` saturates. `retry.rs` owns and tests the schedule, and pinning it here would need a paused-clock test for timing alone. **Timing only.** |
| `provider/model.rs:653` | `OpenAiCompat::groq` → `Default::default()` | `groq()` sets only `supports_usage_in_streaming: true`, which is also the default. **Equivalent.** |
| `provider/model.rs:713` | delete `supports_usage_in_streaming` from `OpenAiCompat::qwen` | Falls back to the default, which is the same `true`. **Equivalent.** It would stop being equivalent if the default ever changed. |
| `provider/prices.rs:777` | `is_zero` → `false` | Used only as `skip_serializing_if`, so `to_json` writes explicit `0.0` rates instead of omitting them. The parse reads both forms the same way. **Serialization verbosity only.** |

Only the two redacting `Debug` impls are excluded in the config. The survivors
above are matched by line and column, which drifts with every edit, so they are
recorded here instead of being written into `exclude_re`.

## First CI run — 2026-09-28

The first `workflow_dispatch` run on `main` (run 36416164104, cargo-mutants
27.1.0) replaces the runtime estimate with measurements. Each mutant took
about 200–210 s to build and 26 s to test, and each shard tested its 117
mutants in 2.2–3.3 h, inside the 320 min cap.

**Four of the eight shards completed:**

| shard | caught | missed | timeout | unviable |
|---|---:|---:|---:|---:|
| 1 | 79 | 20 | 1 | 17 |
| 2 | 74 | 23 | 1 | 19 |
| 4 | 80 | 21 | 2 | 14 |
| 5 | 78 | 23 | 2 | 14 |

**The other four (0, 3, 6, 7) were lost.** Each one received "The runner has
received a shutdown signal" 69–109 min in, with no error from cargo-mutants.
Disk exhaustion is the likely cause: two all-features build trees on about
14 GB free. The workflow now frees the unused preinstalled toolchains before
building, and logs disk and memory before and after the run.

## Reading a scheduled run

The workflow summary lists every missed mutant. For each one:

1. Is it listed above as equivalent or acceptable, with the surrounding code
   unchanged? If so, skip it. Line numbers move, so match on the function and
   the mutation.
2. Is it listed above as a real gap? Then it is already known. Close the gap,
   and update this page when you do.
3. Otherwise it is new. Either a test lost its teeth or new code arrived
   untested. Decide which group it belongs in and add it here.

**Timing.** Locally the run averaged about 55 s per mutant at `-j 2` after a
13-minute cold baseline. At that rate a full run is roughly 14 hours on one
machine. CI splits it into 8 shards of about 116 mutants each. A GitHub
`ubuntu-latest` runner (4 vCPU) is estimated to need about 2.5 minutes per
mutant per job, so about 2.5–3 hours per shard including the cold baseline
build, inside the 330-minute job cap. If shards start approaching the cap, add
shards rather than raising the timeout.
