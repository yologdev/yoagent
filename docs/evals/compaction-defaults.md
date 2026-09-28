# Compaction defaults — offline sweep

Issue [#164](https://github.com/yologdev/yoagent/issues/164) asks for the
compaction constants nobody had measured to be measured instead of chosen, and
[#150](https://github.com/yologdev/yoagent/issues/150) still has one open
question: whether `MIN_HEADROOM_RATIO` causes the post-compaction cliff. This
page reports what an offline sweep of the real compaction code says about each
one.

**No default was changed.** Each section ends in a verdict for the maintainer:
*confirmed*, *change to X* (with the trade-off quantified), or *inconclusive
offline* (with the live experiment that would settle it).

## Method

```text
cargo run --release --example compaction_sweep            # every axis, ~30 s
cargo run --release --example compaction_sweep -- floor   # one axis
```

`examples/compaction_sweep/` generalises `examples/headroom_sweep.rs` (#158).
Like it, it drives the **real** code: `ContextConfig::effective_target_ratio`,
`compact_messages`, `truncate_tool_output_keyed` and `LlmCompaction`. Nothing is
reimplemented. Each request follows the loop's order:

1. measure growth since the last compaction;
2. resolve the ratio;
3. compact, feeding the result back;
4. account for the request;
5. append the response, capping tool output on the way in, as
   `truncate_tool_output_on_append` does by default.

It is deterministic (seeded PRNG; output byte-identical across runs). Each cell
is 5 seeds × 240 requests.

What it adds over `headroom_sweep`:

- **Realistic message shapes.** `headroom_sweep` grew history with pairs of
  *user* messages, which level 1 (tool-output truncation) and level 2
  (summarize old assistant turns) never touch, so every compaction it measured
  was a level-3 cut. Here turns carry assistant tool calls, parallel tool
  results and user messages, so all three tiers and the orphan guards run.
  This changes the picture materially (see the floor section).
- **Four growth profiles**:
  - `chat`: no tools, ~0.7K tokens/turn.
  - `records`: #150's live shape from `long_horizon.rs`, one 60-line record per
    turn, ~1.6K/turn.
  - `coding`: 1–3 parallel calls over `bash` (3–3000 lines, heavy-tailed),
    `read_file` (40–500 lines; exempt from the cap as shipped) and edits,
    ~3.4K/turn after the cap.
  - `mixed`: alternating 10-turn phases of chat and coding.
- **Two effective budgets**: 26K (#150: 30K configured − 4K reserve) and 96K
  (the crate default, 100K − 4K).
- **Retention measures that matter to the model**:
  - whether the opening task prompt survives (`head-`);
  - whether the latest user message survives (`ask-`);
  - how many turns remain in full detail (`turns`);
  - compactions that leave nothing but the marker (`mkr`);
  - dangling tool calls or results (`orph`).
- **An ideal-prefix-cache proxy** (`hit%`, `cost K`): tokens each request
  shares verbatim with the previous one count as hits.
- **`LlmCompaction` with a gated summarizer.** Each summarization request is
  held until the simulation releases it `L` loop turns later. That makes the
  splice race deterministic, and lets it be measured as "how many turns does a
  briefing get".

Column legend (from the run):

```text
cmp/100: compactions per 100 requests. hit%: ideal prefix-cache hit rate. in K: mean
input per request. cost K: input cost per request in K-token-equivalents at Anthropic
cache rates (write 1.25x, read 0.1x). after%/min%: history left after a compaction,
mean/min, as % of budget. turns: turns still present in full detail. head-/ask-:
compactions that lost the opening task prompt / the latest user message. mkr:
compactions that left only the marker. orph: dangling tool calls/results (must be 0).
```

Current values, read from the code at the start of the run (the issue's table
was accurate):

```text
shipped: MIN_HEADROOM_RATIO=0.15 compact_headroom_turns=Some(30) compact_target_ratio=0.7 keep_recent=10 keep_first=2 tool_output_max_lines=200 trigger_ratio=0.6
```

## First: #150's live log, reproduced offline

#150 recorded two compaction outcomes it could not explain in isolation:
`25 msgs / 19504 tok -> 3 msgs / 1665 tok`, and a collapse to
`1 msgs / 22 tok`. `long_horizon` ran `keep_recent: 6`, and its compactions
fired at ~19.5K message tokens (the calibrated budget). The same shapes,
through the real `compact_messages`:

```text
=== #150 reproduction (budget 19.5K calibrated, keep_recent 6, 40 requests) ===

  records — live: 25 msgs / 19504 tok -> 3 msgs / 1665 tok
  shipped floor 0.15:
    request  13: ratio 0.150   26 msgs /  19627 tok ->   3 msgs /   1680 tok   task prompt kept: false
    request  24: ratio 0.150   26 msgs /  19702 tok ->   3 msgs /   1648 tok   task prompt kept: false
    request  35: ratio 0.150   26 msgs /  19704 tok ->   3 msgs /   1652 tok   task prompt kept: false
  floor 0.35:
    request  13: ratio 0.350   26 msgs /  19627 tok ->  17 msgs /   5139 tok   task prompt kept: true
    request  22: ratio 0.350   36 msgs /  19911 tok ->  27 msgs /   5222 tok   task prompt kept: true
    request  31: ratio 0.350   46 msgs /  19958 tok ->  37 msgs /   5383 tok   task prompt kept: true
    request  40: ratio 0.350   55 msgs /  20122 tok ->  46 msgs /   5447 tok   task prompt kept: true

  coding  — live: 22 msgs / 21371 tok -> 1 msgs / 22 tok
  shipped floor 0.15:
    request   7: ratio 0.150   15 msgs /  23310 tok ->   1 msgs /     22 tok   task prompt kept: false
    request  10: ratio 0.150    8 msgs /  21423 tok ->   1 msgs /     22 tok   task prompt kept: false
    request  17: ratio 0.150   19 msgs /  21222 tok ->   1 msgs /     22 tok   task prompt kept: false
    request  22: ratio 0.150   11 msgs /  21097 tok ->   1 msgs /     22 tok   task prompt kept: false
    request  28: ratio 0.150   16 msgs /  19722 tok ->   3 msgs /   1015 tok   task prompt kept: false
    request  33: ratio 0.150   18 msgs /  27145 tok ->   2 msgs /     37 tok   task prompt kept: false
    request  39: ratio 0.150   17 msgs /  19576 tok ->   2 msgs /     37 tok   task prompt kept: false
  floor 0.35:
    request   7: ratio 0.350   15 msgs /  23310 tok ->   1 msgs /     22 tok   task prompt kept: false
    request  10: ratio 0.350    8 msgs /  21423 tok ->   3 msgs /   4558 tok   task prompt kept: false
    request  17: ratio 0.350   21 msgs /  25758 tok ->   1 msgs /     22 tok   task prompt kept: false
    request  22: ratio 0.350   11 msgs /  21097 tok ->   3 msgs /   4338 tok   task prompt kept: false
    request  27: ratio 0.350   16 msgs /  23045 tok ->   1 msgs /     22 tok   task prompt kept: false
    request  33: ratio 0.350   18 msgs /  27145 tok ->   2 msgs /     37 tok   task prompt kept: false
    request  39: ratio 0.350   17 msgs /  19576 tok ->   4 msgs /   3350 tok   task prompt kept: false
```

Both live figures are reproduced, and the mechanism is now clear:

1. **The floor sets the target**: 0.15 × 19.5K ≈ 2.9K.
2. **Level 3 cannot reach it without cutting into the protected tail.** It keeps
   `keep_first` messages and at least `keep_recent` recent messages. When those
   alone exceed the target, it hands off to `keep_within_budget(result, target)`.
3. **`keep_within_budget` drops the head first**, the task prompt included. It
   then keeps only as many of the newest messages as fit in the target, and the
   orphan guard (`safe_tail_start`) drops any tool results whose assistant
   message did not fit. When the newest turn (assistant plus all its parallel
   results) is bigger than the target, **nothing survives but the 22-token
   marker.** That is the `22 → 1` collapse: the model loses even the tool
   results it just asked for.

The `22 → 1` collapse is therefore not a separate bug in the live path. It
comes from `compact_messages`, under the adapted ratio. #150's isolation probe
used `compact_target_ratio: 0.7`, which is why it did not reproduce there.

## `MIN_HEADROOM_RATIO` (0.15)

```text
=== MIN_HEADROOM_RATIO (floor), headroom Some(30) ===

  -- chat @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.15 [current]                 2.9   94.6    14.8    2.41    22.5   13.0   19.4    20%     0%     0     0
  0.20                           2.9   94.6    14.9    2.43    22.9   19.6   19.3     0%     0%     0     0
  0.25                           2.9   94.6    15.3    2.48    25.2   24.3   19.1     0%     0%     0     0
  0.30                           3.2   94.4    15.8    2.60    29.7   28.6   18.5     0%     0%     0     0
  0.35                           3.3   94.3    16.3    2.70    34.4   28.6   17.7     0%     0%     0     0
  0.40                           3.6   94.1    16.9    2.83    38.8   28.6   17.1     0%     0%     0     0
  0.50                           4.2   93.7    17.8    3.07    47.7   28.6   15.6     0%     0%     0     0
  protect-set (cand.)            2.9   94.6    14.9    2.42    22.7   15.6   19.4     0%     0%     0     0
  headroom None (0.7)            5.9   92.3    19.4    3.66    64.3   28.6   13.5     0%     0%     0     0

  -- records (#150) @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.15 [current]                 6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0
  0.20                           7.5   87.1    14.6    3.63    19.1   18.8    8.8   100%    67%     0     0
  0.25                           7.5   87.0    14.6    3.63    19.3   18.9    8.8   100%    64%     0     0
  0.30                           7.9   86.8    15.3    3.87    25.5   25.0    9.3   100%    42%     0     0
  0.35                           8.8   86.5    16.8    4.29    34.6   32.2    9.8     0%     0%     0     0
  0.40                           9.2   86.4    17.0    4.37    37.5   32.2    9.5     0%     0%     0     0
  0.50                           9.2   86.8    17.1    4.31    38.3   32.2    9.5     0%     0%     0     0
  protect-set (cand.)            8.8   86.4    16.4    4.21    33.1   32.2    9.8     0%     0%     0     0
  headroom None (0.7)            9.2   86.8    17.1    4.31    38.3   32.2    9.5     0%     0%     0     0

  -- coding (tool-heavy) @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.15 [current]                11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0
  0.20                          12.3   77.0    12.4    4.54     6.7    0.1    4.6   100%    90%    79     0
  0.25                          12.8   76.9    12.9    4.73    10.3    0.1    4.7   100%    90%    62     0
  0.30                          13.3   77.0    13.5    4.94    15.7    0.1    4.9   100%    82%    43     0
  0.35                          14.0   76.4    13.9    5.16    21.2    0.1    4.9   100%    78%    33     0
  0.40                          15.0   76.4    14.8    5.51    26.7    0.1    5.1   100%    72%    18     0
  0.50                          17.0   75.0    15.6    6.06    36.3    0.1    5.2   100%    71%     8     0
  protect-set (cand.)           26.4   67.3    18.8    8.95    63.3   14.0    5.9    81%    47%     0     0
  headroom None (0.7)           21.6   72.4    18.1    7.55    52.5   14.0    5.7    96%    51%     0     0

  -- mixed @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.15 [current]                 7.0   86.0    12.2    3.20     4.4    0.1    8.1   100%    69%    43     0
  0.20                           7.3   86.2    12.8    3.31     8.6    0.1    8.3   100%    62%    33     0
  0.25                           7.5   85.8    12.6    3.31    11.2    0.1    8.2   100%    64%    31     0
  0.30                           8.1   85.8    13.4    3.52    16.3    0.1    8.4   100%    58%    23     0
  0.35                           8.3   86.0    14.3    3.74    21.8    0.1    8.5    98%    56%    15     0
  0.40                           8.8   85.4    14.7    3.92    26.9    0.1    8.5    96%    50%     8     0
  0.50                           9.9   85.2    15.5    4.17    33.6    0.1    8.5    97%    39%     6     0
  protect-set (cand.)           12.3   82.2    16.6    5.07    49.3   12.1    9.1    44%    23%     0     0
  headroom None (0.7)           11.8   84.3    17.5    4.91    49.7    0.1    8.6    94%    20%     1     0

  -- chat @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.15 [current]                 0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  0.20                           0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  0.25                           0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  0.30                           0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  0.35                           0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  0.40                           0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  0.50                           0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  protect-set (cand.)            0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  headroom None (0.7)            0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0

  -- records (#150) @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.15 [current]                 1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  0.20                           1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  0.25                           1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  0.30                           1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  0.35                           1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  0.40                           1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  0.50                           1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  protect-set (cand.)            1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  headroom None (0.7)            1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0

  -- coding (tool-heavy) @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.15 [current]                 3.7   92.9    50.7    9.21    11.1    4.1   15.1    91%    41%     0     0
  0.20                           3.8   92.9    52.3    9.49    15.3    6.0   15.5    58%    27%     0     0
  0.25                           3.8   93.0    54.0    9.77    18.6    9.5   15.4    26%    13%     0     0
  0.30                           3.8   93.0    54.7    9.85    19.7    9.9   15.4     0%     2%     0     0
  0.35                           3.8   93.1    54.8    9.86    19.7    9.9   15.4     0%     4%     0     0
  0.40                           3.9   93.0    54.9    9.90    20.4    9.9   15.3     0%     0%     0     0
  0.50                           3.9   93.0    54.9    9.90    20.4    9.9   15.3     0%     0%     0     0
  protect-set (cand.)            3.8   92.9    54.0    9.79    18.3    6.0   15.5     0%     2%     0     0
  headroom None (0.7)            3.9   93.0    54.9    9.90    20.4    9.9   15.3     0%     0%     0     0

  -- mixed @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.15 [current]                 2.3   95.7    53.3    7.95    23.3    7.4   23.7     0%     0%     0     0
  0.20                           2.3   95.7    53.3    7.95    23.3    7.4   23.7     0%     0%     0     0
  0.25                           2.3   95.7    53.5    7.97    23.5    7.4   23.7     0%     0%     0     0
  0.30                           2.3   95.8    53.5    7.97    23.9    7.4   23.7     0%     0%     0     0
  0.35                           2.3   95.8    53.5    7.97    23.9    7.4   23.7     0%     0%     0     0
  0.40                           2.3   95.8    53.5    7.97    23.9    7.4   23.7     0%     0%     0     0
  0.50                           2.3   95.8    53.6    7.96    24.4    7.4   23.7     0%     0%     0     0
  protect-set (cand.)            2.3   95.7    53.3    7.95    23.3    7.4   23.7     0%     0%     0     0
  headroom None (0.7)            2.3   95.8    53.6    7.96    24.4    7.4   23.7     0%     0%     0     0
```

The candidate rows:

- **0.20–0.50** apply `real_ratio.max(f)`. That is exactly the shipped code
  with a higher constant, for any `f` ≥ 0.15.
- **protect-set** is *not* shipped code. It raises the ratio to whatever the
  `keep_first` head plus the `keep_recent` tail need (+1%). It approximates a
  structural fix: never let the target fall below the protected set.

What the data says:

- **At the default budget, raising the floor is nearly free, and 0.15 is
  actively harmful for coding agents.**

  | coding @ 96K | cmp/100 | hit% | cost K | head- | ask- |
  |---|---|---|---|---|---|
  | 0.15 (current) | 3.7 | 92.9 | 9.21 | 91% | 41% |
  | 0.30 | 3.8 | 93.0 | 9.85 | 0% | 2% |

  - Under 0.15, 91% of compactions leave a session without its task prompt,
    and 41% without the user's latest message.
  - At 0.30 both go to ~0, with the same compaction count and the same ideal
    hit rate.
  - The +7% input cost is simply the extra history now retained (mean
    post-compaction size 11.1% → 19.7% of budget).
  - `chat` and `records` at 96K are unaffected (identical rows for every
    floor). `mixed` moves by at most 1.1 pt of post-compaction size, with the
    same compaction count.
- **At small budgets no fraction is right, because the protected set is sized
  in messages, not budget.**
  - `records` @ 26K loses the task in 100% of compactions up to a floor of
    0.30, and 0% from 0.35: +31% compactions (6.7 → 8.8 per 100), −0.9 pt
    hit rate.
  - `coding` @ 26K still collapses to the marker at 0.50 (8 marker-only
    compactions). The newest turn alone can exceed half the budget.
  - Only the protect-set candidate removes marker-only collapses everywhere. It
    costs 2.3× the compactions for coding @ 26K, because a 10-message tail of
    bulky results is ~60% of a 26K budget.
- **The cliff at 96K is mostly not the floor.** For `records` @ 96K, the
  post-compaction size is 10.5% of budget under *every* floor, and under
  `headroom None` too. With tool-shaped history, level 2 decides the size, not
  the ratio: it summarizes **every** turn older than `keep_recent`, whatever the
  target. The 8–12K "min after" that `headroom_sweep` reported for `Some(30)`
  came from level-3 cuts, which user-only transcripts force. With real
  transcripts, level 2 usually lands below any target first.

**Verdict: change to 0.30** as the constant-only step. It is zero-cost at 96K
(+0.1 compactions per 100, hit rate unchanged) and ends task-prompt loss there
for tool-heavy work.

It does **not** settle small budgets. What the data suggests instead is a code
change: `level3_drop_middle` should fall back to `keep_within_budget(result,
budget)` rather than `(…, target)`, or otherwise keep the head and the newest
complete turn. In other words, overshoot the target rather than discard the
protected set. The target is a headroom *aim*; only the budget is a hard limit.
The protect-set row is the offline estimate of that behaviour.

## `keep_recent` (10) — and its interaction with the floor

```text
=== keep_recent (messages) ===

  -- chat @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  2                              2.9   94.6    14.9    2.42    22.5   15.4   16.1     0%     0%     0     0
  4                              2.9   94.6    14.9    2.42    22.6   15.3   16.9     0%     0%     0     0
  6                              2.9   94.6    14.9    2.42    22.7   15.3   17.7     0%     0%     0     0
  10 [current]                   2.9   94.6    14.8    2.41    22.5   13.0   19.4    20%     0%     0     0
  16                             2.9   94.6    14.9    2.42    22.1   13.0   21.5    74%     0%     0     0
  24                             2.9   94.6    14.9    2.42    22.0   13.0   22.4   100%     0%     0     0

  -- records (#150) @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  2                              6.7   87.8    13.9    3.34    12.4    7.3    7.6     0%     0%     0     0
  4                              6.7   87.7    14.4    3.49    14.8   13.5    8.4     0%     0%     0     0
  6                              6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0
  10 [current]                   6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0
  16                             6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0
  24                             6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0

  -- coding (tool-heavy) @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  2                             11.6   77.3    12.1    4.37     2.4    0.1    4.3    95%    85%    54     0
  4                             11.7   77.4    12.2    4.37     2.5    0.1    4.4    99%    90%   103     0
  6                             11.7   77.4    12.2    4.38     2.6    0.1    4.5   100%    94%   102     0
  10 [current]                  11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0
  16                            11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0
  24                            11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0

  -- mixed @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  2                              7.0   86.0    12.2    3.20     4.7    0.1    7.6    81%    58%    19     0
  4                              7.1   86.2    12.4    3.22     4.7    0.1    7.8   100%    59%    42     0
  6                              7.0   86.0    12.3    3.21     4.6    0.1    8.0   100%    65%    42     0
  10 [current]                   7.0   86.0    12.2    3.20     4.4    0.1    8.1   100%    69%    43     0
  16                             7.1   86.2    12.4    3.21     4.4    0.1    8.2   100%    67%    45     0
  24                             7.1   86.2    12.4    3.21     4.4    0.1    8.2   100%    67%    45     0

  -- chat @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  2                              0.4   98.5    49.8    5.83    21.7   19.9   62.9     0%     0%     0     0
  4                              0.4   98.5    50.0    5.86    22.4   20.7   63.3     0%     0%     0     0
  6                              0.4   98.5    50.3    5.89    23.0   21.4   63.7     0%     0%     0     0
  10 [current]                   0.4   98.5    50.7    5.94    24.1   22.8   64.5     0%     0%     0     0
  16                             0.4   98.5    51.3    6.00    25.7   24.7   65.6     0%     0%     0     0
  24                             0.4   98.5    52.1    6.09    27.8   26.7   67.2     0%     0%     0     0

  -- records (#150) @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  2                              1.7   96.5    47.2    6.62     3.8    2.6   27.9     0%     0%     0     0
  4                              1.7   96.5    47.6    6.69     5.5    4.3   28.1     0%     0%     0     0
  6                              1.7   96.4    48.0    6.77     7.2    5.9   28.4     0%     0%     0     0
  10 [current]                   1.7   96.4    49.1    6.94    10.5    9.3   29.1     0%     0%     0     0
  16                             1.7   96.4    51.3    7.25    15.6   14.4   30.4     0%     0%     0     0
  24                             1.7   96.5    55.3    7.78    22.3   21.1   32.9     0%     0%     0     0

  -- coding (tool-heavy) @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  2                              3.6   93.1    49.2    8.83     8.6    0.2   13.8     0%     5%     0     0
  4                              3.6   93.1    49.8    8.93     9.1    0.0   14.3    28%    21%     2     0
  6                              3.6   93.0    50.3    9.09    10.8    2.4   14.8    77%    30%     0     0
  10 [current]                   3.7   92.9    50.7    9.21    11.1    4.1   15.1    91%    41%     0     0
  16                             3.7   93.0    51.4    9.30    11.7    4.1   15.5    98%    48%     0     0
  24                             3.7   93.0    51.3    9.29    11.7    4.1   15.6   100%    52%     0     0

  -- mixed @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  2                              2.1   96.0    51.6    7.56    17.0    4.9   21.9     0%     0%     0     0
  4                              2.1   95.9    52.2    7.65    17.8    5.6   22.3     0%     0%     0     0
  6                              2.1   95.9    52.0    7.65    18.3    5.3   23.0     0%     4%     0     0
  10 [current]                   2.3   95.7    53.3    7.95    23.3    7.4   23.7     0%     0%     0     0
  16                             2.4   95.6    53.7    8.10    25.2   10.9   25.4    21%     3%     0     0
  24                             2.5   95.5    55.0    8.37    28.8   15.7   26.6    57%     3%     0     0
```

Joint with the floor (0.15 vs 0.30):

```text
  -- coding (tool-heavy) @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  kr 4 floor 0.15                3.6   93.1    49.8    8.93     9.1    0.0   14.3    28%    21%     2     0
  kr 6 floor 0.15                3.6   93.0    50.3    9.09    10.8    2.4   14.8    77%    30%     0     0
  kr 10 floor 0.15 [current]     3.7   92.9    50.7    9.21    11.1    4.1   15.1    91%    41%     0     0
  kr 16 floor 0.15               3.7   93.0    51.4    9.30    11.7    4.1   15.5    98%    48%     0     0
  kr 4 floor 0.30                3.6   93.2    51.2    9.13    11.3    3.0   14.1     0%     0%     0     0
  kr 6 floor 0.30                3.8   93.1    52.6    9.47    15.9    7.3   14.5     0%     0%     0     0
  kr 10 floor 0.30               3.8   93.0    54.7    9.85    19.7    9.9   15.4     0%     2%     0     0
  kr 16 floor 0.30               4.2   92.6    55.5   10.27    25.0   15.3   16.5    60%    28%     0     0
```

```text
  -- records (#150) @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  kr 4 floor 0.15                6.7   87.7    14.4    3.49    14.8   13.5    8.4     0%     0%     0     0
  kr 6 floor 0.15                6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0
  kr 10 floor 0.15 [current]     6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0
  kr 16 floor 0.15               6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0
  kr 4 floor 0.30                7.1   88.1    14.8    3.52    19.3   13.5    8.1     0%     0%     0     0
  kr 6 floor 0.30                7.9   87.6    15.6    3.80    25.7   19.7    8.6     0%     0%     0     0
  kr 10 floor 0.30               7.9   86.8    15.3    3.87    25.5   25.0    9.3   100%    42%     0     0
  kr 16 floor 0.30               7.9   86.8    15.3    3.87    25.4   25.0    9.3   100%    42%     0     0
```

What the data says:

- **Orphans: 0 in every cell of every axis** (340 deterministic rows, 5 seeds
  each). That includes parallel tool calls. The orphan guards hold.
- **`keep_recent` barely moves compaction count or cache hit rate** (≤ 0.1
  compactions per 100 at 96K). It trades a little retention (`turns`, e.g.
  28.1 → 30.4 for `records` @ 96K from 4 → 16) against how big the protected
  tail is.
- **Its real effect is through the floor.** The bigger the tail, the more
  often it exceeds `floor × budget` and triggers the head-dropping
  `keep_within_budget` path. For `coding` @ 96K: 0% task loss at
  `keep_recent` 2, then 28% at 4, 77% at 6, 91% at 10, 98% at 16. With the
  floor at 0.30, 10 is safe (0%) and 16 is not (60%).

**Verdict: confirmed, conditionally.** 10 is fine once the floor is 0.30 at the
default budget. With the current floor, lower values are safer, but that fixes
the wrong knob. The underlying issue is that a message count and a budget
fraction are different units: a structural fix (above) makes this constant
independent of the floor.

## `keep_first` (2)

```text
  -- chat @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0                              2.9   94.5    14.9    2.42    22.6   13.0   19.4   100%     0%     0     0
  1                              2.9   94.6    14.9    2.42    22.5   13.0   19.4    20%     0%     0     0
  2 [current]                    2.9   94.6    14.8    2.41    22.5   13.0   19.4    20%     0%     0     0
  3                              2.9   94.6    14.9    2.41    22.5   13.0   19.4    20%     0%     0     0
  4                              2.9   94.6    14.9    2.41    22.5   13.0   19.4    20%     0%     0     0
```

```text
  -- coding (tool-heavy) @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0                              3.7   92.9    50.7    9.21    11.1    4.1   15.1    93%    41%     0     0
  1                              3.7   92.9    50.7    9.21    11.1    4.1   15.1    91%    41%     0     0
  2 [current]                    3.7   92.9    50.7    9.21    11.1    4.1   15.1    91%    41%     0     0
  3                              3.7   92.9    50.7    9.21    11.1    4.1   15.1    91%    41%     0     0
  4                              3.7   92.9    50.7    9.21    11.1    4.1   15.1    91%    41%     0     0
```

```text
  -- mixed @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0                              2.3   95.7    53.3    7.95    23.3    7.4   23.7    29%     0%     0     0
  1                              2.3   95.7    53.3    7.95    23.3    7.4   23.7     0%     0%     0     0
  2 [current]                    2.3   95.7    53.3    7.95    23.3    7.4   23.7     0%     0%     0     0
  3                              2.3   95.7    53.3    7.95    23.3    7.4   23.7     0%     0%     0     0
  4                              2.3   95.7    53.3    7.94    23.3    7.4   23.7     0%     0%     0     0
```

- **Values 1–4 are indistinguishable in every cell.** Level 2 runs before
  level 3 computes the head, so by then message 1 (the first assistant turn) is
  already a one-line `[Summary]`. The first tool output never survives,
  whatever `keep_first` is. `safe_head_end` also pulls a head that ends on a
  tool call back to the prompt.
- **0 is harmful**: the task prompt is lost in 100% of `chat` @ 26K
  compactions and 29% of `mixed` @ 96K.
- **Whether the opening turn survives is decided by the floor, not by
  `keep_first`.** The head is only ever dropped by `keep_within_budget`, which
  ignores `keep_first`.

**Verdict: confirmed.** 2 costs nothing over 1 and guards against 0. "Survives
usefully" means the task prompt plus a one-line summary of the first reply,
never the first tool output.

## `compact_target_ratio` (0.7)

```text
  -- chat @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.5 Some(30)                   2.9   94.6    14.8    2.41    22.5   13.0   19.4    20%     0%     0     0
  0.6 Some(30)                   2.9   94.6    14.8    2.41    22.5   13.0   19.4    20%     0%     0     0
  0.7 Some(30) [current]         2.9   94.6    14.8    2.41    22.5   13.0   19.4    20%     0%     0     0
  0.8 Some(30)                   2.9   94.6    14.8    2.41    22.5   13.0   19.4    20%     0%     0     0
  0.9 Some(30)                   2.9   94.6    14.8    2.41    22.5   13.0   19.4    20%     0%     0     0
  0.5 None                       4.2   93.7    17.8    3.07    47.7   28.6   15.6     0%     0%     0     0
  0.6 None                       4.8   93.3    18.7    3.32    56.1   28.6   14.6     0%     0%     0     0
  0.7 None                       5.9   92.3    19.4    3.66    64.3   28.6   13.5     0%     0%     0     0
  0.8 None                       7.6   90.8    20.0    4.11    72.8   28.6   12.6     0%     0%     0     0
  0.9 None                      11.4   88.9    20.5    4.67    82.6   28.6   11.9     0%     0%     0     0
```

```text
  -- coding (tool-heavy) @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph
  0.5 Some(30)                  11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0
  0.6 Some(30)                  11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0
  0.7 Some(30) [current]        11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0
  0.8 Some(30)                  11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0
  0.9 Some(30)                  11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0
  0.5 None                      17.0   75.0    15.6    6.06    36.3    0.1    5.2   100%    71%     8     0
  0.6 None                      19.8   73.7    17.4    6.99    46.8    0.1    5.6   100%    59%     1     0
  0.7 None                      21.6   72.4    18.1    7.55    52.5   14.0    5.7    96%    51%     0     0
  0.8 None                      23.7   70.6    18.6    8.13    57.8   14.0    5.9    95%    45%     0     0
  0.9 None                      26.8   67.7    19.0    8.94    63.7   14.2    5.9    88%    40%     0     0
```

- **Under the default `compact_headroom_turns: Some(30)`, the ratio is inert
  in every cell:** rows 0.5–0.9 are identical in all 8 profile × budget
  sections. The headroom policy's derived ratio is always lower, or level 2
  lands below the target anyway.
- **It only matters with `headroom: None`.** There, lower is better on every
  offline measure except raw token retention. For `chat` @ 26K, 0.5 vs 0.7
  gives 4.2 vs 5.9 compactions per 100, 93.7% vs 92.3% hit rate, 15.6 vs 13.5
  turns in detail, and 3.07 vs 3.66 cost per request.

**Verdict: confirmed** (no change warranted). It is a ceiling that the
shipped headroom policy never reaches. A `None` user would do marginally better
at 0.5–0.6, but that is not the default path.

## `tool_output_max_lines` (200)

```text
  -- coding (tool-heavy) @ 96K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph  g/req trunc% hidden%
  50                             2.8   94.6    50.9    8.27    11.9    0.0   19.6    59%    32%     2     0   2656   23.4    54.1
  100                            3.1   93.8    50.5    8.64    11.7    0.0   17.6    70%    35%     1     0   2954   19.7    48.7
  200 [current]                  3.7   92.9    50.7    9.21    11.1    4.1   15.1    91%    41%     0     0   3416   13.1    40.4
  400                            4.1   92.1    50.8    9.69    10.6    0.0   13.6    98%    71%     3     0   3920    6.9    31.3
  800                            4.6   90.7    47.1    9.75     7.2    0.0   11.7    98%    78%    15     0   4555    4.8    19.8
  off                            5.4   89.4    45.4   10.09     6.4    0.0   10.7    98%    74%    25     0   5654    0.0     0.0
```

```text
  -- coding (tool-heavy) @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph  g/req trunc% hidden%
  50                             9.2   81.2    11.8    3.74     3.4    0.1    5.8   100%    85%    70     0   2656   23.4    54.1
  100                           10.2   79.5    11.7    3.94     2.8    0.1    5.1   100%    93%    85     0   2954   19.7    48.7
  200 [current]                 11.7   77.5    12.2    4.39     2.7    0.1    4.5   100%    95%   102     0   3416   13.1    40.4
  400                           13.2   73.3    11.3    4.59     1.8    0.1    3.8   100%    89%   122     0   3920    6.9    31.3
  800                           14.7   71.6    10.8    4.62     1.4    0.1    3.6   100%    89%   141     0   4555    4.8    19.8
  off                           15.6   72.1    10.2    4.28     1.0    0.1    3.5   100%    90%   153     0   5654    0.0     0.0
```

```text
  -- records (#150) @ 26K --
  value                      cmp/100   hit%    in K  cost K  after%   min%  turns  head-   ask-   mkr  orph  g/req trunc% hidden%
  50                             5.4   90.0    13.9    2.99    11.7   10.1   10.6   100%    69%     0     0   1304  100.0    21.1
  100                            6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0   1646    0.0     0.0
  200 [current]                  6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0   1646    0.0     0.0
  400                            6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0   1646    0.0     0.0
  800                            6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0   1646    0.0     0.0
  off                            6.7   87.4    13.9    3.40    12.8   12.6    8.4   100%    62%     0     0   1646    0.0     0.0
```

- **Every offline metric favours a lower cap.** For `coding` @ 96K, going
  200 → 100 gives:
  - compactions 3.7 → 3.1 per 100;
  - ideal hit rate 92.9% → 93.8%;
  - turns in detail 15.1 → 17.6;
  - cost 9.21 → 8.64.

  Going 200 → 400 or off moves the other way (5.4 compactions per 100 and 10.7
  turns uncapped). Larger single results also push more compactions into the
  floor's head-dropping path: 98% task loss at 400.
- **The price is hidden content.** At 200, 13.1% of coding tool results are
  cut, hiding 40.4% of raw tool-output tokens. At 100 it is 19.7% of results
  and 48.7% of tokens. `records` (60-line outputs) is untouched by any cap of
  100 or more.
- **Retrievability is not the default.** Hidden text is stashed only when a
  `tool_output_sink` is configured (`Agent::with_shared_state`), so by default
  the middle is gone. Where a sink exists, retrievability argues for a
  *lower* cap, not a higher one: hiding becomes cheap. The cost moves to a
  retrieval round trip, plus the full text re-entering context uncapped
  (`shared_state` is exempt).

**Verdict: inconclusive offline; keep 200.** The deciding quantity is how
often the model actually needs a hidden middle, and nothing offline measures
that. The smallest live experiment would run a coding task (like
`long_horizon`'s harness, with bulky `bash` output) with `with_shared_state`
at caps 100 / 200 / 400. Per cap, record:

- `shared_state get` calls on `tool-out-*` keys per truncated result (the
  fetch rate);
- task success;
- session cost.

If the fetch rate at 100 stays low and success holds, lower the cap for
sink-equipped agents first.

## `LlmCompaction::trigger_ratio` (0.6)

Here the real `LlmCompaction` drives the session, and each summarization is
released `L` loop turns after it starts. `win` is the number of turns between a
request starting and the next budget crossing: a briefing splices only if
`L ≤ win`.

```text
  -- records (#150) @ 26K --
  trigger / headroom        L  reqs splice% wasted  win   med  p10 cmp/100  turns
  0.35 Some(30)             1   235    100%      0    2   5.0    2    19.2   10.9
  0.35 Some(30)             2   235    100%      0    2   5.0    2    19.2   10.9
  0.35 Some(30)             3    80     35%     40    2   5.5    2     9.6    8.8
  0.50 Some(30)             1   235    100%      0    3   5.0    3    19.2   11.3
  0.50 Some(30)             2   235    100%      0    3   5.0    3    19.2   11.3
  0.50 Some(30)             3   235    100%      0    3   5.0    3    19.2   11.3
  0.60 Some(30) [current]   1   230    100%      0    5   5.0    5    18.8   11.7
  0.60 Some(30) [current]   2   230    100%      0    5   5.0    5    18.8   11.7
  0.60 Some(30) [current]   3   230    100%      0    5   5.0    5    18.8   11.7
  0.70 Some(30)             1   215    100%      0    4   5.0    4    17.5   11.5
  0.70 Some(30)             2   215    100%      0    4   5.0    4    17.5   11.5
  0.70 Some(30)             3   215    100%      0    4   5.0    4    17.5   11.5
  0.80 Some(30)             1   165    100%      0    3   3.0    3    13.3   10.8
  0.80 Some(30)             2   165    100%      0    3   3.0    3    13.3   10.8
  0.80 Some(30)             3   165    100%      0    3   3.0    3    13.3   10.8
```

```text
  -- coding (tool-heavy) @ 26K --
  trigger / headroom        L  reqs splice% wasted  win   med  p10 cmp/100  turns
  0.35 Some(30)             1   203     85%      0    1   2.0    1    19.9    4.9
  0.35 Some(30)             2   113     48%     32    1   3.0    1    14.2    4.6
  0.35 Some(30)             3    96     33%     43    1   3.0    1    13.2    4.5
  0.50 Some(30)             1   202     84%      0    1   2.0    1    19.9    4.9
  0.50 Some(30)             2   114     49%     31    1   3.0    1    14.2    4.6
  0.50 Some(30)             3    99     32%     46    1   3.0    1    13.4    4.6
  0.60 Some(30) [current]   1   204     83%      0    1   2.0    1    20.1    5.0
  0.60 Some(30) [current]   2   110     44%     34    1   2.0    1    14.2    4.6
  0.60 Some(30) [current]   3    94     29%     47    1   2.0    1    13.4    4.6
  0.70 Some(30)             1   204     83%      0    1   2.0    1    20.2    5.0
  0.70 Some(30)             2   113     42%     38    1   2.0    1    14.6    4.7
  0.70 Some(30)             3    83     20%     50    1   2.0    1    13.1    4.5
  0.80 Some(30)             1   171     73%      0    1   2.0    1    19.2    5.1
  0.80 Some(30)             2    95     32%     40    1   2.0    1    13.9    4.6
  0.80 Some(30)             3    76     13%     55    1   2.0    1    12.8    4.5
```

```text
  -- coding (tool-heavy) @ 96K --
  trigger / headroom        L  reqs splice% wasted  win   med  p10 cmp/100  turns
  0.35 Some(30)             1   105    100%      0    1  12.0    2     8.3   19.7
  0.35 Some(30)             2    97     96%      2    1  11.0    2     7.8   18.8
  0.35 Some(30)             3    83     83%      8    1  10.0    3     7.2   18.2
  0.50 Some(30)             1   104    100%      0    4  10.0    6     8.2   20.7
  0.50 Some(30)             2   104    100%      0    4  10.0    6     8.2   20.7
  0.50 Some(30)             3   104    100%      0    4  10.0    6     8.2   20.7
  0.60 Some(30) [current]   1   102    100%      0    4  10.0    6     8.2   20.9
  0.60 Some(30) [current]   2   102    100%      0    4  10.0    6     8.2   20.9
  0.60 Some(30) [current]   3   102    100%      0    4  10.0    6     8.2   20.9
  0.70 Some(30)             1    85    100%      0    4   8.0    5     6.8   19.8
  0.70 Some(30)             2    85    100%      0    4   8.0    5     6.8   19.8
  0.70 Some(30)             3    85    100%      0    4   8.0    5     6.8   19.8
  0.80 Some(30)             1    70    100%      0    2   5.0    3     5.7   18.5
  0.80 Some(30)             2    70    100%      0    2   5.0    3     5.7   18.5
  0.80 Some(30)             3    66     94%      2    2   5.0    3     5.5   18.2
```

(Rows with `compact_headroom_turns: None` are in the full output. They differ
only where the deterministic fallback runs.)

What the data says:

- **At 96K, 0.6 splices 100% for summarizers up to 3 turns slow**, in every
  profile. 0.35 and 0.8 are the values that start to lose splices (coding:
  83% and 94% at `L = 3`).
- **The trigger barely changes the window where it is short.** For `coding` @
  26K the median window is 2 turns at every trigger, and splice rates for
  `L = 1/2/3` are 83/44/29% at 0.6 against 85/48/33% at 0.35.

  This is the offline explanation of #150's "0.6 and 0.35 behaved
  identically". After a splice, history is still well above the trigger, so
  the next request starts at the splice itself. The window is then
  (budget − post-splice size) / growth, which the trigger cannot move.
- **Post-splice history is large, so `LlmCompaction` compacts more often.**
  The cut is fixed when the request *starts*, so the retained tail includes
  all growth until the splice. At 0.6, `records` @ 26K compacts 18.8 times per
  100 requests against 6.7 for the deterministic path, and `coding` @ 96K 8.2
  against 3.7.
  - This contradicts the module docs' "6 vs 6" rewrites, which
    `prefix_cache_harness` measured on text-only turns; worth re-measuring
    with tool-shaped history.
  - A higher trigger trades the other way: 0.8 cuts compactions to 13.3 and
    5.7 but shrinks the window (median 3 and 5 turns).
- **For #150's shape, the window is not the reason every compaction fell
  back.** At `records` @ 26K the minimum window at 0.6 is 5 turns, and
  splicing is 100% for `L ≤ 3`. #150's live 100% fallback rate is better
  explained by the two causes already fixed in 0.18.1: the retry underflow
  panic and a slow loop-model summarizer.

**Verdict: confirmed.** 0.6 is at or next to the best splice rate and retention
in every cell. What decides whether briefings land is summarizer latency against
growth rate, not the trigger. **Inconclusive offline:** `L` in wall-clock terms.
The smallest live experiment is `long_horizon` with a fast summarizer (Haiku)
and a slow one (the loop model), timing summarization requests against loop
turns.

## #150: conclusion

- **Does the `MIN_HEADROOM_RATIO` floor cause the cliff described?** Yes, for
  what #150 actually observed: the `3 msgs / 1665 tok` and `1 msg / 22 tok`
  results and the lost task (`LOST`). Both are reproduced offline at the floor,
  through `keep_within_budget(…, target)`, which drops the head and, when the
  newest turn exceeds the target, everything except the marker. At the default
  96K budget, though, the post-compaction *size* in tool sessions is mostly set
  by level 2, not the floor.
- **What the data suggests instead:**
  - Raise the floor to 0.30. It is free at 96K and ends task loss there for
    tool-heavy work.
  - More durably, never let the headroom target discard the protected set:
    fall back against the budget, not the target. That is the only variant
    that removes marker-only collapses at small budgets.
  - Separately, level 2's all-or-nothing summarization is the remaining cliff
    at 96K. It could summarize the oldest turns only until the target is met,
    mirroring level 3's smallest-span rule.
- **Does this settle #150?** It settles the *mechanism* and the "not
  explained" `22 → 1` collapse, and it answers the floor question offline.
  #150's own acceptance criterion, **prefix-cache hit rate on a real provider**
  under the current vs a higher floor, still needs a live run. The ideal-cache
  proxy puts the difference at ≤ 0.1 pt at 96K, and −0.9 pt for `records` @
  26K at the 0.35 floor it needs. That is small enough that it would not
  justify keeping the current floor. The live check:

  ```text
  YO_HEADROOM=30 ANTHROPIC_API_KEY=... cargo run --example long_horizon
  ```

  run once as shipped and once with the floor at 0.30. That needs the constant
  changed on a branch, since it is not configurable. Compare the cache-read
  totals and the final recall answer.

## Caveats

- **Ideal cache.** `hit%` counts every verbatim-shared prefix token as a hit.
  Real providers cache at breakpoints, in blocks, with TTLs. It ranks
  configurations; it does not predict a live rate.
- **Fixed calibrated budget.** The loop re-derives the budget from provider
  usage every turn; the sweep holds it fixed (`system_prompt_tokens: 0`).
- **Synthetic content.** Sizes follow the stated distributions, and token
  counts use the crate's own chars/4 estimate. What the model would *do* with
  less history (re-fetch, re-ask, fail) is not modelled. `head-` and `ask-` say
  what it no longer sees, not what that costs.
- **Task loss is sticky.** Once the prompt is dropped it never returns, so one
  early loss counts against every later compaction.
- **Latency in turns.** Summarizer latency is a whole number of loop turns.
  Real latency is wall-clock and varies.
- **Candidates are candidates.** "protect-set" approximates a code change; it is
  not an implementation of one.

Numbers from commit `cdbb3df`, run with the command above.
