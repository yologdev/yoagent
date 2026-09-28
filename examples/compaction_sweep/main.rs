//! Offline sweep of the compaction defaults that have never been measured —
//! [#164](https://github.com/yologdev/yoagent/issues/164), and the
//! post-compaction cliff still open on
//! [#150](https://github.com/yologdev/yoagent/issues/150).
//!
//! ```text
//! cargo run --release --example compaction_sweep            # every axis
//! cargo run --release --example compaction_sweep -- floor   # one axis
//! ```
//!
//! About 30 s in release on a laptop; output is byte-identical run to run.
//!
//! Axes: `repro150` (#150's live compaction log, reproduced offline), `floor`
//! (`MIN_HEADROOM_RATIO`), `ratio` (`compact_target_ratio`), `keep_recent`,
//! `joint` (`keep_recent` x floor), `keep_first`, `max_lines`
//! (`tool_output_max_lines`), `trigger`
//! (`LlmCompaction`'s `trigger_ratio`). Results and verdicts:
//! `docs/evals/compaction-defaults.md`.
//!
//! # Why one example, not one per axis
//!
//! Every axis needs the same three things — a transcript generator that emits
//! what the loop really appends, a driver that calls the compaction code in
//! the loop's order, and the same metrics — so per-axis examples would be six
//! copies of the same ~500 lines. The axes differ only in which knob moves.
//!
//! # What this is, and is not
//!
//! Like `headroom_sweep`, it drives the real `effective_target_ratio`,
//! `compact_messages`, `truncate_tool_output_keyed` and `LlmCompaction` — no
//! reimplementation — so every number is a pure function of the shipped code,
//! deterministic, and free. Unlike `headroom_sweep` it uses realistic message
//! *shapes* (assistant tool calls, parallel tool results, user turns), so all
//! three compaction tiers and the orphan-avoidance rules run.
//!
//! Prefix-cache figures assume an **ideal** cache: everything the previous
//! request shared verbatim is a hit. That is an upper bound — real providers
//! cache at breakpoints, in blocks, with a TTL — so it ranks configurations
//! but does not predict a live hit rate. Summarizer latency is modelled in
//! whole loop turns.

mod profiles;
mod sim;

use profiles::Profile;
use sim::{simulate, Knobs, LlmKnobs, Metrics};
use yoagent::context::{ContextConfig, MIN_HEADROOM_RATIO};
use yoagent::llm_compaction::DEFAULT_TRIGGER_RATIO;

/// Requests per session. Long enough for several compactions at the default
/// budget under tool-heavy growth.
const REQUESTS: usize = 240;
/// Seeds per cell; figures are summed/averaged over them.
const SEEDS: u64 = 5;
/// Effective message budgets: #150's live run (30K configured, 26K after the
/// 4K `system_prompt_tokens` reserve) and the crate default (100K − 4K).
const BUDGETS: [usize; 2] = [26_000, 96_000];

/// Run one session on its own single-threaded runtime. Sessions share
/// nothing, so running them on separate threads cannot change a result.
fn run(profile: Profile, seed: u64, requests: usize, k: &Knobs) -> Metrics {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio runtime")
        .block_on(simulate(profile, seed, requests, k))
}

/// All seeds of one cell, one thread per seed, summed in seed order.
fn cell(profile: Profile, k: &Knobs) -> Metrics {
    let per_seed: Vec<Metrics> = std::thread::scope(|s| {
        let handles: Vec<_> = (1..=SEEDS)
            .map(|seed| s.spawn(move || run(profile, seed, REQUESTS, k)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("simulation thread"))
            .collect()
    });
    let mut total = Metrics::default();
    for m in &per_seed {
        total.add(m);
    }
    total
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        100.0 * n as f64 / d as f64
    }
}

fn header(extra: &str) {
    println!(
        "  {:<26} {:>7} {:>6} {:>7} {:>7} {:>7} {:>6} {:>6} {:>6} {:>6} {:>5} {:>5}{extra}",
        "value",
        "cmp/100",
        "hit%",
        "in K",
        "cost K",
        "after%",
        "min%",
        "turns",
        "head-",
        "ask-",
        "mkr",
        "orph",
    );
}

fn row(label: &str, budget: usize, m: &Metrics, extra: &str) {
    let req = m.requests.max(1) as f64;
    let uncached = m.input - m.cached;
    let cost = (1.25 * uncached as f64 + 0.1 * m.cached as f64) / req / 1000.0;
    let after = if m.compactions == 0 {
        0.0
    } else {
        100.0 * m.after_sum as f64 / m.compactions as f64 / budget as f64
    };
    println!(
        "  {:<26} {:>7.1} {:>6.1} {:>7.1} {:>7.2} {:>7.1} {:>6.1} {:>6.1} {:>5.0}% {:>5.0}% {:>5} {:>5}{extra}",
        label,
        100.0 * m.compactions as f64 / req,
        pct(m.cached, m.input),
        m.input as f64 / req / 1000.0,
        cost,
        after,
        100.0 * m.after_min as f64 / budget as f64,
        m.detail_turns as f64 / req,
        pct(m.head_lost, m.compactions),
        pct(m.ask_lost, m.compactions),
        m.marker_only,
        m.orphans,
    );
}

fn section(profile: Profile, budget: usize) {
    println!("\n  -- {} @ {}K --", profile.name(), budget / 1000);
}

type Variant = (String, Box<dyn Fn(&mut Knobs)>);

fn sweep(title: &str, variants: Vec<Variant>) {
    println!("\n=== {title} ===");
    for budget in BUDGETS {
        for profile in Profile::ALL {
            section(profile, budget);
            header("");
            for (label, apply) in &variants {
                let mut k = Knobs::defaults(budget);
                apply(&mut k);
                let m = cell(profile, &k);
                row(label, budget, &m, "");
            }
        }
    }
}

fn mark(label: String, current: bool) -> String {
    if current {
        format!("{label} [current]")
    } else {
        label
    }
}

/// #150's live compaction log, reproduced offline.
///
/// `long_horizon` ran `keep_recent: 6` against a 30K configured budget, and its
/// compactions fired at ~19.5K message tokens — the calibrated budget after the
/// measured request overhead. Same shape here, through the real code.
fn repro150() {
    println!("\n=== #150 reproduction (budget 19.5K calibrated, keep_recent 6, 40 requests) ===");
    for (profile, label) in [
        (
            Profile::Records,
            "records — live: 25 msgs / 19504 tok -> 3 msgs / 1665 tok",
        ),
        (
            Profile::Coding,
            "coding  — live: 22 msgs / 21371 tok -> 1 msgs / 22 tok",
        ),
    ] {
        for floor in [None, Some(0.35f32)] {
            let mut k = Knobs::defaults(19_500);
            k.keep_recent = 6;
            k.floor = floor;
            k.trace = true;
            match floor {
                None => println!("\n  {label}\n  shipped floor {MIN_HEADROOM_RATIO}:"),
                Some(f) => println!("  floor {f}:"),
            }
            run(profile, 1, 40, &k);
        }
    }
}

fn floor_axis() {
    let mut v: Vec<Variant> = vec![(
        format!("{MIN_HEADROOM_RATIO} [current]"),
        Box::new(|_: &mut Knobs| {}),
    )];
    for f in [0.20f32, 0.25, 0.30, 0.35, 0.40, 0.50] {
        v.push((
            format!("{f:.2}"),
            Box::new(move |k: &mut Knobs| k.floor = Some(f)),
        ));
    }
    v.push((
        "protect-set (cand.)".into(),
        Box::new(|k: &mut Knobs| k.protect = true),
    ));
    v.push((
        "headroom None (0.7)".into(),
        Box::new(|k: &mut Knobs| k.headroom = None),
    ));
    sweep("MIN_HEADROOM_RATIO (floor), headroom Some(30)", v);
}

fn ratio_axis() {
    let d = ContextConfig::default().compact_target_ratio;
    let mut v: Vec<Variant> = Vec::new();
    for headroom in [ContextConfig::default().compact_headroom_turns, None] {
        for r in [0.5f32, 0.6, 0.7, 0.8, 0.9] {
            let tag = match headroom {
                Some(n) => format!("{r:.1} Some({n})"),
                None => format!("{r:.1} None"),
            };
            let current = (r - d).abs() < 1e-6 && headroom.is_some();
            v.push((
                mark(tag, current),
                Box::new(move |k: &mut Knobs| {
                    k.target_ratio = r;
                    k.headroom = headroom;
                }),
            ));
        }
    }
    sweep("compact_target_ratio", v);
}

fn keep_recent_axis() {
    let d = ContextConfig::default().keep_recent;
    let v: Vec<Variant> = [2usize, 4, 6, 10, 16, 24]
        .into_iter()
        .map(|n| {
            (
                mark(n.to_string(), n == d),
                Box::new(move |k: &mut Knobs| k.keep_recent = n) as Box<dyn Fn(&mut Knobs)>,
            )
        })
        .collect();
    sweep("keep_recent (messages)", v);
}

/// `keep_recent` under the shipped floor and under a raised one. The two
/// interact: `keep_recent` sizes the protected tail, and the floor decides
/// whether the compaction target leaves room for it.
fn joint_axis() {
    let d = ContextConfig::default().keep_recent;
    let mut v: Vec<Variant> = Vec::new();
    for floor in [None, Some(0.30f32)] {
        for n in [4usize, 6, 10, 16] {
            let tag = match floor {
                None => format!("kr {n} floor {MIN_HEADROOM_RATIO}"),
                Some(f) => format!("kr {n} floor {f:.2}"),
            };
            v.push((
                mark(tag, n == d && floor.is_none()),
                Box::new(move |k: &mut Knobs| {
                    k.keep_recent = n;
                    k.floor = floor;
                }),
            ));
        }
    }
    sweep("keep_recent x MIN_HEADROOM_RATIO", v);
}

fn keep_first_axis() {
    let d = ContextConfig::default().keep_first;
    let v: Vec<Variant> = [0usize, 1, 2, 3, 4]
        .into_iter()
        .map(|n| {
            (
                mark(n.to_string(), n == d),
                Box::new(move |k: &mut Knobs| k.keep_first = n) as Box<dyn Fn(&mut Knobs)>,
            )
        })
        .collect();
    sweep("keep_first (messages)", v);
}

fn max_lines_axis() {
    println!("\n=== tool_output_max_lines (append-path cap; read_file exempt) ===");
    println!("  g/req: tokens appended per request after the cap. trunc%: tool results cut.");
    println!("  hidden%: share of raw tool-output tokens behind a marker — retrievable via");
    println!("  shared_state when a tool_output_sink is configured, gone otherwise.");
    let d = ContextConfig::default().tool_output_max_lines;
    for budget in BUDGETS {
        for profile in [Profile::Records, Profile::Coding, Profile::Mixed] {
            section(profile, budget);
            header(&format!(" {:>6} {:>6} {:>7}", "g/req", "trunc%", "hidden%"));
            for n in [50usize, 100, 200, 400, 800, usize::MAX] {
                let mut k = Knobs::defaults(budget);
                k.max_lines = n;
                let m = cell(profile, &k);
                let label = if n == usize::MAX {
                    "off".to_string()
                } else {
                    mark(n.to_string(), n == d)
                };
                let extra = format!(
                    " {:>6.0} {:>6.1} {:>7.1}",
                    m.appended as f64 / m.requests as f64,
                    pct(m.truncated, m.tool_results),
                    pct(m.tool_tokens_raw - m.tool_tokens_kept, m.tool_tokens_raw),
                );
                row(&label, budget, &m, &extra);
            }
        }
    }
}

fn median(v: &mut [usize]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] + v[n / 2]) as f64 / 2.0
    }
}

fn trigger_axis() {
    println!("\n=== LlmCompaction trigger_ratio (real LlmCompaction, gated summarizer) ===");
    println!("  L: turns the summarizer takes. win: turns from a request starting to the next");
    println!("  budget crossing (min / median / p10). splice%: compactions that spliced a");
    println!("  briefing. reqs: summarization requests billed. wasted: billed, never spliced\n  (requests still in flight when the session ends are not counted as wasted).");
    for budget in BUDGETS {
        for profile in [Profile::Records, Profile::Coding, Profile::Chat] {
            section(profile, budget);
            println!(
                "  {:<24} {:>2} {:>5} {:>7} {:>6} {:>4} {:>5} {:>4} {:>7} {:>6}",
                "trigger / headroom",
                "L",
                "reqs",
                "splice%",
                "wasted",
                "win",
                "med",
                "p10",
                "cmp/100",
                "turns"
            );
            for headroom in [ContextConfig::default().compact_headroom_turns, None] {
                for trigger in [0.35f32, 0.5, 0.6, 0.7, 0.8] {
                    for latency in [1usize, 2, 3] {
                        let mut k = Knobs::defaults(budget);
                        k.headroom = headroom;
                        k.llm = Some(LlmKnobs { trigger, latency });
                        let m = cell(profile, &k);
                        let h = match headroom {
                            Some(n) => format!("Some({n})"),
                            None => "None".to_string(),
                        };
                        let current =
                            (trigger - DEFAULT_TRIGGER_RATIO).abs() < 1e-6 && headroom.is_some();
                        let label = mark(format!("{trigger:.2} {h}"), current);
                        let min = m.windows.iter().min().copied().unwrap_or(0);
                        let mut w = m.windows.clone();
                        let med = median(&mut w);
                        w.sort_unstable();
                        let p10 = w.get(w.len() / 10).copied().unwrap_or(0);
                        println!(
                            "  {:<24} {:>2} {:>5} {:>6.0}% {:>6} {:>4} {:>5.1} {:>4} {:>7.1} {:>6.1}",
                            label,
                            latency,
                            m.llm_requests,
                            pct(m.splices, m.llm_compactions),
                            m.windows.len().saturating_sub(m.splices),
                            min,
                            med,
                            p10,
                            100.0 * m.llm_compactions as f64 / m.requests as f64,
                            m.detail_turns as f64 / m.requests as f64,
                        );
                    }
                }
            }
        }
    }
}

fn main() {
    let axis = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let d = ContextConfig::default();
    println!("Compaction defaults sweep — #164 / #150");
    println!(
        "shipped: MIN_HEADROOM_RATIO={MIN_HEADROOM_RATIO} compact_headroom_turns={:?} \
         compact_target_ratio={} keep_recent={} keep_first={} tool_output_max_lines={} \
         trigger_ratio={DEFAULT_TRIGGER_RATIO}",
        d.compact_headroom_turns,
        d.compact_target_ratio,
        d.keep_recent,
        d.keep_first,
        d.tool_output_max_lines
    );
    println!(
        "{REQUESTS} requests x {SEEDS} seeds per cell; budgets {BUDGETS:?} (effective message \
         budget)."
    );
    println!(
        "cmp/100: compactions per 100 requests. hit%: ideal prefix-cache hit rate. in K: mean\n\
         input per request. cost K: input cost per request in K-token-equivalents at Anthropic\n\
         cache rates (write 1.25x, read 0.1x). after%/min%: history left after a compaction,\n\
         mean/min, as % of budget. turns: turns still present in full detail. head-/ask-:\n\
         compactions that lost the opening task prompt / the latest user message. mkr:\n\
         compactions that left only the marker. orph: dangling tool calls/results (must be 0)."
    );

    let t0 = std::time::Instant::now();
    let all = axis == "all";
    if all || axis == "repro150" {
        repro150();
    }
    if all || axis == "floor" {
        floor_axis();
    }
    if all || axis == "ratio" {
        ratio_axis();
    }
    if all || axis == "keep_recent" {
        keep_recent_axis();
    }
    if all || axis == "joint" {
        joint_axis();
    }
    if all || axis == "keep_first" {
        keep_first_axis();
    }
    if all || axis == "max_lines" {
        max_lines_axis();
    }
    if all || axis == "trigger" {
        trigger_axis();
    }
    eprintln!("\n(sweep took {:.1}s)", t0.elapsed().as_secs_f64());
}
