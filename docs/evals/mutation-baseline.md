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
| CI | [`.github/workflows/mutants.yml`](https://github.com/yologdev/yoagent/blob/main/.github/workflows/mutants.yml): weekly (Mon 03:17 UTC) + manual, 16 round-robin shards |
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
A second run, after freeing about 20 GB of disk, lost the same four shards,
although the runners had 110 GB free. So disk was not the cause.

The cause was deterministic. `level2_summarize_old_turns` advances with
`i += 1` and pushes a summary or a cloned message on every step. Mutating
any of its four increments to `-=` or `*=` makes it revisit a message
forever, pushing each time. The test process then grows until the 16 GB
runner is shut down, before cargo-mutants' 60 s timeout fires. Round-robin
sharding placed those eight mutants in shards 0, 3, 6 and 7. They are now
excluded in `.cargo/mutants.toml`, with this reason. They are detected
rather than missed, because any test that reaches level 2 hangs on them.
With them excluded there are 928 mutants.

## First full run — 2026-09-28

Run [36480327733](https://github.com/yologdev/yoagent/actions/runs/36480327733)
tested all 928 mutants at commit `4feda1e`, with 16 shards, incremental builds
and the runner-killing mutants excluded. It took 30 minutes. Every shard
finished in 20–29 minutes; a mutant now takes about 11–33 s to build and
20–26 s to test.

| module | caught | missed | timeout | unviable | total | detected |
|---|---:|---:|---:|---:|---:|---:|
| `agent_loop.rs` | 68 | 39 | 6 | 25 | 138 | 65% |
| `context.rs` | 186 | 69 | 2 | 12 | 269 | 73% |
| `llm_compaction.rs` | 86 | 45 | 0 | 22 | 153 | 65% |
| `provider/model.rs` | 144 | 40 | 0 | 38 | 222 | 78% |
| `provider/prices.rs` | 64 | 9 | 0 | 17 | 90 | 87% |
| `provider/prices/global.rs` | 39 | 0 | 0 | 17 | 56 | 100% |
| **total** | **587** | **202** | **8** | **131** | **928** | **75%** |

"Detected" means (caught + timeout) / (caught + missed + timeout); unviable
mutants do not compile and are left out.

This run supersedes the 1-in-10 sample above as the baseline to compare
against. The sample's triage still applies: its 13 real gaps and 7 acceptable
survivors all appear in the list below. The other survivors have not been
triaged yet. The list is kept here in full, because a run's artifacts expire
after 30 days.

<details>
<summary>All 202 missed mutants (sorted by file and line)</summary>

```text
src/agent_loop.rs:209:9: delete field prompts from struct LoopScope expression in with_loop_scope
src/agent_loop.rs:500:16: delete ! in run_loop
src/agent_loop.rs:556:28: replace > with < in run_loop
src/agent_loop.rs:556:28: replace > with == in run_loop
src/agent_loop.rs:556:28: replace > with >= in run_loop
src/agent_loop.rs:562:25: replace += with *= in run_loop
src/agent_loop.rs:588:36: replace += with *= in run_loop
src/agent_loop.rs:589:34: replace += with *= in run_loop
src/agent_loop.rs:591:57: replace > with < in run_loop
src/agent_loop.rs:591:57: replace > with == in run_loop
src/agent_loop.rs:591:57: replace > with >= in run_loop
src/agent_loop.rs:592:41: replace / with * in run_loop
src/agent_loop.rs:592:41: replace / with % in run_loop
src/agent_loop.rs:623:30: replace != with == in run_loop
src/agent_loop.rs:633:29: delete field compact_target_ratio from struct ContextConfig expression in run_loop
src/agent_loop.rs:715:76: replace - with / in run_loop
src/agent_loop.rs:927:37: delete match arm Message::Assistant{usage, ..} in run_loop
src/agent_loop.rs:1085:44: delete ! in run_loop
src/agent_loop.rs:1118:38: replace + with * in run_loop
src/agent_loop.rs:1118:53: replace + with * in run_loop
src/agent_loop.rs:1118:72: replace + with - in run_loop
src/agent_loop.rs:1128:21: delete match arm Message::Assistant{usage, ..} in run_loop
src/agent_loop.rs:1157:20: delete ! in run_loop
src/agent_loop.rs:1282:21: delete match arm StreamEvent::ThinkingDelta{delta, ..} in stream_assistant_response
src/agent_loop.rs:1294:21: delete match arm StreamEvent::ToolCallDelta{delta, ..} in stream_assistant_response
src/agent_loop.rs:1311:21: delete match arm StreamEvent::Error{message} in stream_assistant_response
src/agent_loop.rs:1430:14: replace match guard *name == schema.name with true in unwrap_structured_tool_call
src/agent_loop.rs:1451:36: replace == with != in unwrap_structured_tool_call
src/agent_loop.rs:1510:24: delete ! in execute_tool_calls
src/agent_loop.rs:1513:51: replace + with - in execute_tool_calls
src/agent_loop.rs:1513:51: replace + with * in execute_tool_calls
src/agent_loop.rs:1513:56: replace * with / in execute_tool_calls
src/agent_loop.rs:1513:56: replace * with + in execute_tool_calls
src/agent_loop.rs:1514:37: replace < with <= in execute_tool_calls
src/agent_loop.rs:1514:37: replace < with == in execute_tool_calls
src/agent_loop.rs:1514:37: replace < with > in execute_tool_calls
src/agent_loop.rs:1555:16: delete ! in execute_sequential
src/agent_loop.rs:1557:66: replace + with - in execute_sequential
src/agent_loop.rs:1557:66: replace + with * in execute_sequential
src/context.rs:33:71: replace + with * in message_tokens
src/context.rs:35:80: replace + with - in message_tokens
src/context.rs:35:80: replace + with * in message_tokens
src/context.rs:48:44: replace * with / in content_tokens
src/context.rs:48:44: replace * with + in content_tokens
src/context.rs:48:48: replace / with * in content_tokens
src/context.rs:48:48: replace / with % in content_tokens
src/context.rs:49:28: replace / with * in content_tokens
src/context.rs:49:28: replace / with % in content_tokens
src/context.rs:56:43: replace + with * in content_tokens
src/context.rs:56:85: replace * with / in content_tokens
src/context.rs:56:85: replace * with + in content_tokens
src/context.rs:56:89: replace / with * in content_tokens
src/context.rs:56:89: replace / with % in content_tokens
src/context.rs:56:93: replace / with * in content_tokens
src/context.rs:56:93: replace / with % in content_tokens
src/context.rs:60:40: replace + with * in content_tokens
src/context.rs:110:67: replace + with * in ContextTracker::record_usage
src/context.rs:111:18: replace > with >= in ContextTracker::record_usage
src/context.rs:124:48: replace match guard idx < messages.len() with true in ContextTracker::estimate_context_tokens
src/context.rs:124:52: replace < with <= in ContextTracker::estimate_context_tokens
src/context.rs:134:9: replace ContextTracker::reset with ()
src/context.rs:319:23: replace || with && in ContextConfig::effective_target_ratio
src/context.rs:319:62: replace || with && in ContextConfig::effective_target_ratio
src/context.rs:437:17: replace < with <= in compact_messages
src/context.rs:437:17: replace < with == in compact_messages
src/context.rs:437:17: replace < with > in compact_messages
src/context.rs:451:24: replace != with == in compact_messages
src/context.rs:487:5: replace message_text -> String with "xyzzy".into()
src/context.rs:487:5: replace message_text -> String with String::new()
src/context.rs:530:14: replace ^= with |= in fnv1a
src/context.rs:717:13: replace < with <= in level2_summarize_old_turns
src/context.rs:727:25: delete match arm Content::Text{text} in level2_summarize_old_turns
src/context.rs:728:43: replace > with < in level2_summarize_old_turns
src/context.rs:728:43: replace > with == in level2_summarize_old_turns
src/context.rs:728:43: replace > with >= in level2_summarize_old_turns
src/context.rs:743:34: delete ! in level2_summarize_old_turns
src/context.rs:745:38: replace > with < in level2_summarize_old_turns
src/context.rs:745:38: replace > with == in level2_summarize_old_turns
src/context.rs:745:38: replace > with >= in level2_summarize_old_turns
src/context.rs:764:25: replace < with <= in level2_summarize_old_turns
src/context.rs:764:25: replace < with == in level2_summarize_old_turns
src/context.rs:764:25: replace < with > in level2_summarize_old_turns
src/context.rs:813:5: replace message_timestamp -> u64 with 0
src/context.rs:813:5: replace message_timestamp -> u64 with 1
src/context.rs:825:5: replace opens_tool_calls -> bool with false
src/context.rs:840:5: replace safe_head_end -> usize with 1
src/context.rs:840:15: replace > with < in safe_head_end
src/context.rs:840:15: replace > with == in safe_head_end
src/context.rs:840:15: replace > with >= in safe_head_end
src/context.rs:841:13: replace -= with /= in safe_head_end
src/context.rs:841:13: replace -= with += in safe_head_end
src/context.rs:849:17: replace < with > in safe_tail_start
src/context.rs:850:15: replace += with -= in safe_tail_start
src/context.rs:858:17: replace > with >= in safe_turn_start
src/context.rs:859:15: replace -= with += in safe_turn_start
src/context.rs:880:17: replace >= with < in level3_drop_middle
src/context.rs:902:23: replace - with / in level3_drop_middle
src/context.rs:902:23: replace - with + in level3_drop_middle
src/context.rs:918:30: replace > with >= in level3_drop_middle
src/context.rs:935:19: replace > with >= in keep_within_budget
src/context.rs:938:19: replace -= with /= in keep_within_budget
src/context.rs:946:14: replace > with >= in keep_within_budget
src/context.rs:1009:9: replace ExecutionLimits::with_max_turns -> Self with Default::default()
src/context.rs:1043:5: replace signature_hash -> u64 with 0
src/context.rs:1043:5: replace signature_hash -> u64 with 1
src/context.rs:1044:7: replace ^= with &= in signature_hash
src/context.rs:1044:7: replace ^= with |= in signature_hash
src/context.rs:1181:30: replace > with >= in ExecutionTracker::record_tool_calls
src/llm_compaction.rs:367:5: replace safe_head_boundary -> usize with 1
src/llm_compaction.rs:403:9: replace Phase::is_idle -> bool with true
src/llm_compaction.rs:474:9: replace InflightGuard::disarm with ()
src/llm_compaction.rs:551:9: replace <impl Drop for LlmCompaction>::drop with ()
src/llm_compaction.rs:630:20: replace != with == in LlmCompaction::with_trigger_ratio
src/llm_compaction.rs:675:19: replace < with <= in LlmCompaction::with_max_summary_tokens
src/llm_compaction.rs:675:19: replace < with == in LlmCompaction::with_max_summary_tokens
src/llm_compaction.rs:675:19: replace < with > in LlmCompaction::with_max_summary_tokens
src/llm_compaction.rs:737:9: replace LlmCompaction::summary_cost -> Option<f64> with None
src/llm_compaction.rs:764:19: replace > with >= in LlmCompaction::choose_cut
src/llm_compaction.rs:764:45: replace < with <= in LlmCompaction::choose_cut
src/llm_compaction.rs:773:16: replace > with >= in LlmCompaction::choose_cut
src/llm_compaction.rs:773:34: replace - with / in LlmCompaction::choose_cut
src/llm_compaction.rs:773:34: replace - with + in LlmCompaction::choose_cut
src/llm_compaction.rs:780:41: replace < with <= in LlmCompaction::choose_cut
src/llm_compaction.rs:780:63: replace + with - in LlmCompaction::choose_cut
src/llm_compaction.rs:780:63: replace + with * in LlmCompaction::choose_cut
src/llm_compaction.rs:872:16: delete ! in LlmCompaction::spawn_summarize
src/llm_compaction.rs:912:13: delete field enabled from struct CacheConfig expression in LlmCompaction::spawn_summarize
src/llm_compaction.rs:998:13: delete field system_prompt_tokens from struct ContextConfig expression in LlmCompaction::shrink_tail
src/llm_compaction.rs:1082:48: replace < with > in summarize
src/llm_compaction.rs:1090:76: replace + with - in summarize
src/llm_compaction.rs:1090:76: replace + with * in summarize
src/llm_compaction.rs:1100:20: replace < with > in summarize
src/llm_compaction.rs:1101:64: replace + with - in summarize
src/llm_compaction.rs:1101:64: replace + with * in summarize
src/llm_compaction.rs:1151:17: replace > with >= in <impl CompactionStrategy for LlmCompaction>::compact
src/llm_compaction.rs:1165:49: replace - with / in <impl CompactionStrategy for LlmCompaction>::compact
src/llm_compaction.rs:1165:49: replace - with + in <impl CompactionStrategy for LlmCompaction>::compact
src/llm_compaction.rs:1168:46: replace > with >= in <impl CompactionStrategy for LlmCompaction>::compact
src/llm_compaction.rs:1170:50: replace > with >= in <impl CompactionStrategy for LlmCompaction>::compact
src/llm_compaction.rs:1252:17: replace > with >= in <impl CompactionStrategy for LlmCompaction>::compact
src/llm_compaction.rs:1286:19: replace <= with > in clip
src/llm_compaction.rs:1291:15: replace > with < in clip
src/llm_compaction.rs:1291:15: replace > with == in clip
src/llm_compaction.rs:1291:15: replace > with >= in clip
src/llm_compaction.rs:1291:19: replace && with || in clip
src/llm_compaction.rs:1292:13: replace -= with /= in clip
src/llm_compaction.rs:1292:13: replace -= with += in clip
src/llm_compaction.rs:1327:25: delete match arm Content::Image{..} in serialize_transcript
src/llm_compaction.rs:1335:25: delete match arm Content::Text{text} in serialize_transcript
src/llm_compaction.rs:1340:25: delete match arm Content::ToolCall{name, arguments, ..} in serialize_transcript
src/llm_compaction.rs:1346:25: delete match arm Content::Thinking{..} in serialize_transcript
src/llm_compaction.rs:1383:5: replace assistant_usage -> Usage with Default::default()
src/llm_compaction.rs:1384:9: delete match arm Message::Assistant{usage, ..} in assistant_usage
src/provider/model.rs:215:9: replace ContextTier::is_configured -> bool with true
src/provider/model.rs:215:32: replace != with == in ContextTier::is_configured
src/provider/model.rs:216:13: replace || with && in ContextTier::is_configured
src/provider/model.rs:216:40: replace != with == in ContextTier::is_configured
src/provider/model.rs:217:13: replace || with && in ContextTier::is_configured
src/provider/model.rs:217:44: replace != with == in ContextTier::is_configured
src/provider/model.rs:218:13: replace || with && in ContextTier::is_configured
src/provider/model.rs:218:45: replace != with == in ContextTier::is_configured
src/provider/model.rs:504:5: replace warn_effort_clamp with ()
src/provider/model.rs:601:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::openai
src/provider/model.rs:617:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::meta
src/provider/model.rs:644:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::xai
src/provider/model.rs:653:9: replace OpenAiCompat::groq -> Self with Default::default()
src/provider/model.rs:654:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::groq
src/provider/model.rs:661:9: replace OpenAiCompat::cerebras -> Self with Default::default()
src/provider/model.rs:666:9: replace OpenAiCompat::openrouter -> Self with Default::default()
src/provider/model.rs:667:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::openrouter
src/provider/model.rs:668:13: delete field max_tokens_field from struct Self expression in OpenAiCompat::openrouter
src/provider/model.rs:675:9: replace OpenAiCompat::mistral -> Self with Default::default()
src/provider/model.rs:676:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::mistral
src/provider/model.rs:677:13: delete field max_tokens_field from struct Self expression in OpenAiCompat::mistral
src/provider/model.rs:687:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::deepseek
src/provider/model.rs:688:13: delete field max_tokens_field from struct Self expression in OpenAiCompat::deepseek
src/provider/model.rs:696:9: replace OpenAiCompat::zai -> Self with Default::default()
src/provider/model.rs:697:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::zai
src/provider/model.rs:704:9: replace OpenAiCompat::minimax -> Self with Default::default()
src/provider/model.rs:705:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::minimax
src/provider/model.rs:713:13: delete field supports_usage_in_streaming from struct Self expression in OpenAiCompat::qwen
src/provider/model.rs:714:13: delete field max_tokens_field from struct Self expression in OpenAiCompat::qwen
src/provider/model.rs:856:47: replace < with > in claude_version::leading_digits
src/provider/model.rs:1283:13: delete field max_tokens from struct Self expression in ModelConfig::claude_fable_5
src/provider/model.rs:1396:13: delete field max_tokens from struct Self expression in ModelConfig::claude_opus_4_8
src/provider/model.rs:1409:13: delete field context_window from struct Self expression in ModelConfig::claude_sonnet_5
src/provider/model.rs:1410:13: delete field max_tokens from struct Self expression in ModelConfig::claude_sonnet_5
src/provider/model.rs:1426:13: delete field context_window from struct Self expression in ModelConfig::claude_haiku_4_5
src/provider/model.rs:1427:13: delete field max_tokens from struct Self expression in ModelConfig::claude_haiku_4_5
src/provider/model.rs:1467:13: delete field reasoning from struct Self expression in ModelConfig::gpt_5_5
src/provider/model.rs:1469:13: delete field max_tokens from struct Self expression in ModelConfig::gpt_5_5
src/provider/model.rs:1569:13: delete field reasoning from struct Self expression in ModelConfig::gpt_6
src/provider/model.rs:1571:13: delete field max_tokens from struct Self expression in ModelConfig::gpt_6
src/provider/prices.rs:403:38: replace || with && in is_iso_date
src/provider/prices.rs:640:17: delete match arm (None, None) in diff_tables
src/provider/prices.rs:721:5: replace describe -> String with "xyzzy".into()
src/provider/prices.rs:721:5: replace describe -> String with String::new()
src/provider/prices.rs:728:8: delete ! in describe
src/provider/prices.rs:735:5: replace describe_tiers -> String with "xyzzy".into()
src/provider/prices.rs:735:5: replace describe_tiers -> String with String::new()
src/provider/prices.rs:777:5: replace is_zero -> bool with false
src/provider/prices.rs:781:5: replace is_false -> bool with false
```

</details>

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
machine. The first CI runs, with 8 shards of about 116 mutants, took about
231 s per mutant (205 s of it building) and 2.2–3.3 hours per shard. The
build dominated because rust-cache exports `CARGO_INCREMENTAL=0` for the whole
job, so every mutant rebuilt the crate and relinked every test binary from
scratch. The workflow now re-enables incremental builds for the mutants step
and uses 16 shards of about 58 mutants each. If shards start approaching the
200-minute step cap, add shards rather than raising the timeout.
