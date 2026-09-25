//! Price drift audit — do this crate's hardcoded token prices still match
//! reality?
//!
//! This crate's prices live in `src/provider/prices.json`, compiled into the
//! crate; every priced preset and first-party constructor reads it. When a
//! vendor reprices, the built-in data keeps the old numbers until someone
//! edits the file (users can override at runtime; see
//! `docs/concepts/pricing.md`), and nothing detects the gap. That is
//! not hypothetical: `claude_sonnet_5` carried Sonnet **4.6's** rates —
//! $3/$15 against the published $2/$10 — from **v0.9.0 through v0.16.5**,
//! 18 tagged releases, overstating every `cost_usd` for that model by 50%.
//! It was found by someone asking, not by any mechanism.
//!
//! It is **still uncorrected on the `release/0.16.x` maintenance line**, which
//! does not carry this test — a 0.16.6 would re-ship it.
//!
//! ```text
//! cargo test --test price_audit -- --ignored --nocapture
//! ```
//!
//! Run it before a release, at step 3 of the release checklist.
//!
//! # Why models.dev, and what it is not
//!
//! [models.dev](https://github.com/anomalyco/models.dev) is an MIT-licensed,
//! schema-validated database served as JSON. It carries `cache_read` and
//! `cache_write`, which this crate needs and which many aggregators omit —
//! cache pricing is the subject of the compaction and telemetry work.
//!
//! It is **community-maintained and therefore not authoritative.** A failure
//! here is a *drift alarm* that sends a human to the vendor's own pricing page.
//! If the two disagree, the answer is "go read Anthropic", never "copy
//! models.dev". This test deliberately does not, and should never, update the
//! data file for you.
//!
//! # How this instrument avoids going quiet
//!
//! An audit that reports "clean" because it compared nothing is worse than no
//! audit — it converts an open question into false assurance. Review of the
//! first version found six distinct ways to reach a green pass having checked
//! zero fields: a renamed provider key, a restructured envelope, a renamed
//! `models`/`cost` level, an HTTP error with a JSON body, `null`, or `{}`.
//! Each is plausible for a community database with ~190 providers.
//!
//! So the pass condition is not "no drift found". It is:
//!
//! - **every field is accounted for** — compared against a number, or recorded
//!   as absent upstream. A count short of the expected total fails.
//! - **every `prices.json` entry is found** unless it carries an explicit, dated
//!   `absent_upstream` note. A rename silently dropping a model out
//!   of coverage is the failure this catches.
//! - **absent is not zero.** A missing `cache_write` prints `—`, never `0`, so
//!   the table never claims a comparison it did not make. A rate models.dev
//!   omits must be `0` in the crate, with one asserted exception:
//!   the entry's `cache_write_at_input` flag, for a vendor with no separate
//!   cache-write charge whose entry states the input rate explicitly. That
//!   allowance holds only while the crate's rate equals the band's input rate
//!   *and* models.dev still omits the field; either changing is drift.
//! - **unknown cost keys fail.** models.dev carries structure this audit may
//!   not understand; ignoring it would certify an entry that is knowably
//!   wrong somewhere.
//! - **tiers are compared, not waved through.** An entry with context tiers
//!   is checked tier by tier — threshold and all four rates — against
//!   models.dev's `tiers` array (and its `context_over_200k` mirror). A tiered
//!   entry against a flat upstream, or the reverse, or a different number of
//!   tiers, is drift.
//! - **HTTP status is checked**, and a non-JSON body reports the status,
//!   content type and first bytes rather than a bare parse error.

use yoagent::provider::{CostConfig, ModelConfig, PriceTable};

const DB_URL: &str = "https://models.dev/api.json";

/// Cost keys this audit understands. Anything else means models.dev is
/// describing pricing structure this audit does not compare.
const KNOWN_COST_KEYS: [&str; 4] = ["input", "output", "cache_read", "cache_write"];

/// models.dev's context-tier keys. Understood — and compared — only for an
/// entry that is itself tiered; on a flat entry they are unknown structure
/// and fail like any other unrecognised key.
///
/// `context_over_200k` is models.dev's older single-tier encoding, still
/// emitted beside `tiers` with the same rates (whatever its name says, on the
/// OpenAI entries it mirrors a 272K tier). It is compared against the
/// entry's first tier so the two encodings cannot drift apart unnoticed.
const TIER_KEYS: [&str; 2] = ["tiers", "context_over_200k"];

/// The fewest entries `prices.json` may hold. `expected` below is derived
/// from the entries themselves, so it cannot notice the file shrinking — an
/// empty table produced "0 compared, 0 drifted" and a green pass. Raise this
/// when you add an entry.
const MIN_ENTRIES: usize = 13;

/// A `prices.json` entry and where to look when it drifts.
struct Preset {
    /// Where to edit, so a failure names the entry: `prices.json provider/id`.
    constructor: String,
    /// models.dev provider key (ours, mapped where the two differ).
    provider: String,
    /// models.dev model key — the same as ours.
    model: String,
    /// The vendor's own page (the entry's `source`) — the authority when the
    /// two disagree.
    vendor_page: String,
    cost: CostConfig,
    /// The entry's `absent_upstream`: set only when models.dev genuinely
    /// lacks this model, with the date it was checked by hand. `None` means
    /// "must be present": absence is a failure, because a rename dropping a
    /// model out of coverage looks exactly like a database that has not
    /// caught up yet.
    absent_upstream: Option<String>,
    /// Set when models.dev lists cost structure a flat `CostConfig` cannot
    /// express. Records *exactly* what was acknowledged, so the note cannot
    /// become a blanket amnesty for whatever appears later. Audit-local (see
    /// [`FLAT_RATE_GAPS`]), not data: it records a decision about models.dev.
    flat_rate_gap: Option<&'static FlatRateGap>,
    /// The entry's `cache_write_at_input`: the vendor charges nothing extra
    /// for cache writes, models.dev omits `cache_write`, and the entry states
    /// it explicitly as the input rate (what `CostConfig::cost_usd` would
    /// bill an unset rate at anyway). Without this, "absent upstream ⇒ must
    /// be 0" would force the entry to leave the rate implicit.
    ///
    /// Asserted, not waived: in every band, the crate's `cache_write` must
    /// equal that band's `input`, and models.dev must still omit the field.
    /// If upstream starts listing it, or the crate's rate is 0 or anything
    /// but the input rate, the allowance is stale and the audit fails.
    cache_write_at_input: bool,
    /// The entry's `note`, printed with the results — e.g. a rate that
    /// matches upstream but that the vendor's page does not itself state.
    note: Option<String>,
}

/// A recorded, verified gap between what models.dev carries and what a flat
/// `CostConfig` can express.
///
/// Prose alone made this a waiver: the audit checked only that a note existed,
/// so a *new* unknown key — a second tier, an audio rate, a reasoning rate —
/// folded silently into the old note and the entry stayed green forever. The
/// reverse went unnoticed too: if the upstream claim vanished, or its key was
/// renamed (indistinguishable from removal), nothing said the recorded decision
/// had gone stale.
///
/// So the acknowledgement names the keys and the rates it was made against, and
/// both directions are assertions.
///
/// No entry records one today: `gpt-5.5`, the only one that ever did, is now
/// tiered and compared tier by tier. Kept for the next genuine disagreement.
#[allow(dead_code)]
struct FlatRateGap {
    /// Cost keys this gap covers. Any *other* unknown key is drift.
    keys: &'static [&'static str],
    /// Upstream rates the decision was reasoned against, as
    /// `(json_pointer, value)`. A revision makes the recorded reasoning stale.
    rates: &'static [(&'static str, f64)],
    why: &'static str,
}

/// Recorded gaps, as `(provider, model, gap)`. Empty today.
const FLAT_RATE_GAPS: &[(&str, &str, FlatRateGap)] = &[];

/// models.dev's provider key for one of ours. The two agree except where
/// models.dev names the vendor differently.
fn upstream_provider(ours: &str) -> &str {
    match ours {
        "qwen" => "alibaba",
        other => other,
    }
}

/// Every entry of the built-in `prices.json`, as an audit row. The audit
/// covers the data file, not a hand-kept list of presets, so an entry cannot
/// be added without being audited.
fn presets() -> Vec<Preset> {
    PriceTable::builtin()
        .iter()
        .map(|(provider, model, entry)| Preset {
            constructor: format!("prices.json {provider}/{model}"),
            provider: upstream_provider(provider).to_string(),
            model: model.to_string(),
            vendor_page: entry
                .source
                .clone()
                .unwrap_or_else(|| panic!("prices.json {provider}/{model} has no `source`")),
            cost: entry.cost.clone(),
            absent_upstream: entry.absent_upstream.clone(),
            flat_rate_gap: FLAT_RATE_GAPS
                .iter()
                .find(|(p, m, _)| *p == provider && *m == model)
                .map(|(_, _, gap)| gap),
            cache_write_at_input: entry.cache_write_at_input,
            note: entry.note.clone(),
        })
        .collect()
}

/// Every priced preset constructor must resolve through an audited entry.
/// Independent of `presets()`, which is derived from the data file: a preset
/// that stopped reading the table (a literal creeping back into
/// `model.rs`) would be priced but unaudited.
#[test]
fn every_priced_preset_is_an_audited_entry() {
    let table = PriceTable::builtin();
    for config in [
        ModelConfig::claude_fable_5(),
        ModelConfig::claude_fable_5_1(),
        ModelConfig::claude_opus_5_5(),
        ModelConfig::claude_opus_5(),
        ModelConfig::claude_opus_4_8(),
        ModelConfig::claude_sonnet_5(),
        ModelConfig::claude_haiku_4_5(),
        ModelConfig::gpt_5_5(),
        ModelConfig::gpt_6_astra(),
        ModelConfig::gpt_6_sol(),
        ModelConfig::gpt_6_luna(),
        ModelConfig::meta("muse-spark-1.1", "Muse Spark 1.1"),
        ModelConfig::meta("muse-spark-1.2", "Muse Spark 1.2"),
    ] {
        let entry = table
            .entry(&config.provider, &config.id)
            .unwrap_or_else(|| {
                panic!("{}/{} has no prices.json entry", config.provider, config.id)
            });
        assert_eq!(
            config.cost.as_ref(),
            Some(&entry.cost),
            "{} is not priced from its entry",
            config.id
        );
    }
    assert!(presets().len() >= MIN_ENTRIES);
}

/// What models.dev says about one cost field.
///
/// Absent is deliberately not folded into `0.0`: a preset whose rate is
/// legitimately zero would then be unauditable on that field forever, and the
/// operator-facing table would print a comparison that never happened.
#[derive(Debug)]
enum Upstream {
    Value(f64),
    Absent,
    /// Present but not a number — the schema retyped, which is always drift.
    Malformed(String),
}

fn field(cost: &serde_json::Value, name: &str) -> Upstream {
    match cost.get(name) {
        None | Some(serde_json::Value::Null) => Upstream::Absent,
        Some(v) => match v.as_f64() {
            Some(n) => Upstream::Value(n),
            None => Upstream::Malformed(v.to_string()),
        },
    }
}

/// The verdict on a field models.dev does not list: `None` if the crate's
/// value is acceptable, else the drift message.
///
/// Absent upstream means the crate must carry `0` — except a `cache_write`
/// covered by [`Preset::cache_write_at_input`], which must then equal
/// `input`, the same band's input rate, exactly. An allowance on a preset
/// whose rate is `0` is stale too: it is recorded for a rate that is not
/// there.
fn check_absent(p: &Preset, field_name: &str, ours: f64, input: f64) -> Option<String> {
    let is_cache_write = field_name.ends_with("cache_write");
    match (is_cache_write, p.cache_write_at_input) {
        (true, true) if ours == 0.0 => Some(format!(
            "{}: {field_name} is 0 in the crate but `cache_write_at_input` is recorded for \
             {} — the allowance is stale; set the rate to the input rate or delete it.",
            p.model, p.constructor
        )),
        (true, true) if (ours - input).abs() < 1e-9 => None,
        (true, true) => Some(format!(
            "{}: {field_name} is {ours} in the crate; `cache_write_at_input` allows an \
             unlisted cache_write only at the band's input rate ({input}). Check {}.",
            p.model, p.vendor_page
        )),
        _ if ours != 0.0 => Some(format!(
            "{}: {field_name} is {ours} in the crate and models.dev does not list \
             it — one of the two is wrong. Check {}.",
            p.model, p.vendor_page
        )),
        _ => None,
    }
}

/// The reverse direction of [`check_absent`]: a `cache_write_at_input`
/// allowance on a field models.dev *does* list has gone stale.
fn stale_allowance(p: &Preset, field_name: &str) -> Option<String> {
    (field_name.ends_with("cache_write") && p.cache_write_at_input).then(|| {
        format!(
            "{}: models.dev now lists {field_name}, so the `cache_write_at_input` \
             allowance on {} is stale — the value is compared directly now; delete \
             the allowance.",
            p.model, p.constructor
        )
    })
}

#[tokio::test]
#[ignore = "network: fetches models.dev; run before a release"]
async fn hardcoded_prices_have_not_drifted() {
    let resp = reqwest::get(DB_URL)
        .await
        .unwrap_or_else(|e| panic!("GET {DB_URL} failed: {e}"));
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    let body = resp.text().await.expect("read models.dev body");
    let head = &body[..body.len().min(300)];

    // `reqwest::get` returns Ok for 4xx/5xx, so without this a JSON error
    // envelope parses cleanly and every lookup misses — a green run that
    // compared nothing.
    assert!(
        status.is_success(),
        "GET {DB_URL} -> {status} ({content_type})\nfirst 300 bytes:\n{head}"
    );
    let db: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!(
            "GET {DB_URL} -> {status} ({content_type}) is not JSON: {e}\nfirst 300 bytes:\n{head}"
        )
    });

    let mut drift: Vec<String> = Vec::new();
    let mut unexpectedly_missing: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let (mut compared, mut absent) = (0usize, 0usize);

    println!(
        "\n{:<30} {:<14} {:>10} {:>12}",
        "model", "field", "yoagent", "models.dev"
    );
    println!("{}", "-".repeat(72));

    let all = presets();
    for p in &all {
        let Some(cost) = db
            .get(p.provider.as_str())
            .and_then(|v| v.get("models"))
            .and_then(|v| v.get(p.model.as_str()))
            .and_then(|v| v.get("cost"))
        else {
            match &p.absent_upstream {
                Some(why) => {
                    absent += 4;
                    notes.push(format!("{} absent upstream — {why}", p.model));
                }
                None => unexpectedly_missing.push(format!(
                    "{} ({}/{}) — was covered, now absent. A rename or removal drops it \
                     silently out of coverage; confirm at {} and either fix the key or \
                     set `absent_upstream` with today's date and a reason.",
                    p.constructor, p.provider, p.model, p.vendor_page
                )),
            }
            continue;
        };

        // Structure this audit does not understand is a reason to fail, not to
        // read past. Tiered rates mean the flat preset is wrong somewhere.
        if let Some(obj) = cost.as_object() {
            let tiered = !p.cost.context_tiers.is_empty();
            let unknown: Vec<String> = obj
                .keys()
                .filter(|k| !KNOWN_COST_KEYS.contains(&k.as_str()))
                .filter(|k| !(tiered && TIER_KEYS.contains(&k.as_str())))
                .cloned()
                .collect();

            match p.flat_rate_gap {
                Some(gap) => {
                    // Anything beyond what was acknowledged is new structure,
                    // not covered by an old decision. Without this the note is
                    // a blanket amnesty: a second tier or an audio rate would
                    // fold into it and the preset would stay green forever.
                    let unacknowledged: Vec<&String> = unknown
                        .iter()
                        .filter(|k| !gap.keys.contains(&k.as_str()))
                        .collect();
                    if !unacknowledged.is_empty() {
                        drift.push(format!(
                            "{}: models.dev carries cost keys beyond the recorded gap: \
                             {unacknowledged:?}. The note covers {:?} only. Check {} and \
                             either fix {} or widen the acknowledgement.",
                            p.model, gap.keys, p.vendor_page, p.constructor
                        ));
                    }

                    // The reverse is just as important. If the claim vanishes —
                    // or a key is renamed, which is indistinguishable from
                    // removal — the recorded decision has gone stale and
                    // nothing would otherwise say so.
                    let missing: Vec<&&str> =
                        gap.keys.iter().filter(|k| !obj.contains_key(**k)).collect();
                    if !missing.is_empty() {
                        drift.push(format!(
                            "{}: the recorded gap names {missing:?}, which models.dev no \
                             longer carries. Either upstream dropped the claim — re-check \
                             {} and delete the note — or a key was renamed and this preset \
                             is now silently uncovered.",
                            p.model, p.vendor_page
                        ));
                    }

                    // And the rates the decision was reasoned against.
                    for (pointer, expected) in gap.rates {
                        match cost.pointer(pointer).and_then(|v| v.as_f64()) {
                            Some(actual) if (actual - expected).abs() < 1e-9 => {}
                            other => drift.push(format!(
                                "{}: recorded gap expects {pointer} == {expected}, models.dev \
                                 now says {other:?}. The decision in {} was reasoned against \
                                 the old figure; re-check {}.",
                                p.model, p.constructor, p.vendor_page
                            )),
                        }
                    }
                    notes.push(format!("{}: {}", p.model, gap.why));
                }
                None if !unknown.is_empty() => drift.push(format!(
                    "{}: models.dev carries cost keys this audit ignores: {unknown:?}. \
                     `CostConfig` is one flat rate — if those are tiers, {} is wrong \
                     above the boundary. Check {}, then either fix the preset or record \
                     it in `flat_rate_gap`.",
                    p.model, p.constructor, p.vendor_page
                )),
                None => {}
            }
        }

        for (name, ours) in [
            ("input", p.cost.input_per_million),
            ("output", p.cost.output_per_million),
            ("cache_read", p.cost.cache_read_per_million),
            ("cache_write", p.cost.cache_write_per_million),
        ] {
            match field(cost, name) {
                Upstream::Value(theirs) => {
                    compared += 1;
                    drift.extend(stale_allowance(p, name));
                    let same = (ours - theirs).abs() < 1e-9;
                    println!(
                        "{:<30} {:<14} {:>10} {:>12}  {}",
                        p.model,
                        name,
                        ours,
                        theirs,
                        if same { "ok" } else { "DRIFT" }
                    );
                    if !same {
                        drift.push(format!(
                            "{}: {name} is {ours} in the crate, {theirs} in models.dev — \
                             check {} and edit {} if the vendor agrees",
                            p.model, p.vendor_page, p.constructor
                        ));
                    }
                }
                Upstream::Absent => {
                    absent += 1;
                    // `—`, never `0`: the table must not claim a comparison it
                    // did not make.
                    println!(
                        "{:<30} {:<14} {:>10} {:>12}  not listed upstream",
                        p.model, name, ours, "—"
                    );
                    drift.extend(check_absent(p, name, ours, p.cost.input_per_million));
                }
                Upstream::Malformed(raw) => {
                    compared += 1;
                    drift.push(format!(
                        "{}: {name} is {raw} in models.dev, not a number — the schema \
                         changed and this audit's key path needs re-deriving.",
                        p.model
                    ));
                }
            }
        }

        // Context tiers, when the preset has any: threshold and every rate of
        // every tier. A flat preset never reaches here — upstream tiers on it
        // were reported as unknown keys above.
        if !p.cost.context_tiers.is_empty() {
            let upstream_tiers = cost.get("tiers").and_then(|t| t.as_array());
            let Some(upstream_tiers) = upstream_tiers else {
                drift.push(format!(
                    "{}: {} is tiered at {:?} but models.dev carries no `tiers` array. \
                     Either upstream flattened it or the key moved — check {}.",
                    p.model,
                    p.constructor,
                    p.cost
                        .context_tiers
                        .iter()
                        .map(|t| t.above_prompt_tokens)
                        .collect::<Vec<_>>(),
                    p.vendor_page
                ));
                // Account for the fields as compared-and-failed so the
                // coverage assertion does not fire on top of the real error.
                compared += 5 * p.cost.context_tiers.len();
                continue;
            };
            if upstream_tiers.len() != p.cost.context_tiers.len() {
                drift.push(format!(
                    "{}: {} has {} context tier(s), models.dev has {}. Check {}.",
                    p.model,
                    p.constructor,
                    p.cost.context_tiers.len(),
                    upstream_tiers.len(),
                    p.vendor_page
                ));
            }
            for (i, ours) in p.cost.context_tiers.iter().enumerate() {
                let label = format!("tier{i}");
                let tier_input = ours.input_per_million;
                let Some(theirs) = upstream_tiers.get(i) else {
                    compared += 5;
                    continue;
                };
                let size = theirs.pointer("/tier/size").and_then(|v| v.as_f64());
                compared += 1;
                let same_size = size == Some(ours.above_prompt_tokens as f64);
                println!(
                    "{:<30} {:<14} {:>10} {:>12}  {}",
                    p.model,
                    format!("{label}.above"),
                    ours.above_prompt_tokens,
                    size.map_or("—".into(), |s| s.to_string()),
                    if same_size { "ok" } else { "DRIFT" }
                );
                if !same_size {
                    drift.push(format!(
                        "{}: {label} starts above {} prompt tokens in the crate, models.dev \
                         says {size:?}. Check {}.",
                        p.model, ours.above_prompt_tokens, p.vendor_page
                    ));
                }
                for (name, ours) in [
                    ("input", ours.input_per_million),
                    ("output", ours.output_per_million),
                    ("cache_read", ours.cache_read_per_million),
                    ("cache_write", ours.cache_write_per_million),
                ] {
                    let field_name = format!("{label}.{name}");
                    match field(theirs, name) {
                        Upstream::Value(v) => {
                            compared += 1;
                            drift.extend(stale_allowance(p, &field_name));
                            let same = (ours - v).abs() < 1e-9;
                            println!(
                                "{:<30} {:<14} {:>10} {:>12}  {}",
                                p.model,
                                field_name,
                                ours,
                                v,
                                if same { "ok" } else { "DRIFT" }
                            );
                            if !same {
                                drift.push(format!(
                                    "{}: {field_name} is {ours} in the crate, {v} in models.dev \
                                     — check {} and edit {} if the vendor agrees",
                                    p.model, p.vendor_page, p.constructor
                                ));
                            }
                        }
                        Upstream::Absent => {
                            absent += 1;
                            println!(
                                "{:<30} {:<14} {:>10} {:>12}  not listed upstream",
                                p.model, field_name, ours, "—"
                            );
                            drift.extend(check_absent(p, &field_name, ours, tier_input));
                        }
                        Upstream::Malformed(raw) => {
                            compared += 1;
                            drift.push(format!(
                                "{}: {field_name} is {raw} in models.dev, not a number.",
                                p.model
                            ));
                        }
                    }
                }
            }
            // The legacy mirror must agree with the first tier. Not counted
            // toward coverage — it duplicates `tiers[0]` — but a disagreement
            // is drift either way.
            if let (Some(mirror), Some(first)) =
                (cost.get("context_over_200k"), p.cost.context_tiers.first())
            {
                for (name, ours) in [
                    ("input", first.input_per_million),
                    ("output", first.output_per_million),
                    ("cache_read", first.cache_read_per_million),
                    ("cache_write", first.cache_write_per_million),
                ] {
                    let theirs = match field(mirror, name) {
                        Upstream::Value(v) => v,
                        // The same rule as the base and tier fields.
                        Upstream::Absent => {
                            let field_name = format!("context_over_200k.{name}");
                            drift.extend(check_absent(
                                p,
                                &field_name,
                                ours,
                                first.input_per_million,
                            ));
                            continue;
                        }
                        Upstream::Malformed(raw) => {
                            drift.push(format!(
                                "{}: context_over_200k.{name} is {raw}, not a number.",
                                p.model
                            ));
                            continue;
                        }
                    };
                    if (ours - theirs).abs() >= 1e-9 {
                        drift.push(format!(
                            "{}: context_over_200k.{name} is {theirs} in models.dev but the \
                             crate's first tier says {ours}. Check {}.",
                            p.model, p.vendor_page
                        ));
                    }
                }
            }
        }
        if let Some(n) = &p.note {
            notes.push(format!("{}: {n}", p.model));
        }
        if p.cache_write_at_input {
            notes.push(format!(
                "{}: cache_write at input rate (allowance recorded in prices.json)",
                p.model
            ));
        }
    }

    println!("{}", "-".repeat(72));
    println!(
        "{compared} compared, {absent} not listed upstream, {} drifted, {} unexpectedly missing",
        drift.len(),
        unexpectedly_missing.len()
    );
    for n in &notes {
        println!("  note: {n}");
    }

    // A rename must not quietly reduce coverage.
    assert!(
        unexpectedly_missing.is_empty(),
        "\n\nPresets vanished from models.dev:\n\n{}\n",
        unexpectedly_missing.join("\n")
    );

    // The load-bearing assertion. Without it, every schema change at or above
    // the `cost` level yields drift.is_empty() == true and a green pass having
    // verified nothing.
    assert!(
        all.len() >= MIN_ENTRIES,
        "\n\nThe audit is checking {} entries but prices.json shipped at least {MIN_ENTRIES}. \
         A price is unaudited — its rates can drift for as many releases as it takes \
         someone to notice, which for `claude_sonnet_5` was 18.\n",
        all.len()
    );

    // Four base rates per preset, plus threshold + four rates per tier.
    let expected: usize = all.iter().map(|p| 4 + 5 * p.cost.context_tiers.len()).sum();
    assert_eq!(
        compared + absent,
        expected,
        "\n\nThe audit accounted for {} of {expected} fields. It is not reporting \
         clean prices — it is reporting nothing. models.dev's schema or hosting \
         changed underneath this test; re-derive the key path against {DB_URL}.\n",
        compared + absent
    );

    assert!(
        drift.is_empty(),
        "\n\nPrice drift detected. models.dev is community-maintained and NOT \
         authoritative — confirm against the vendor page before changing any \
         constant, and never copy models.dev blindly.\n\n{}\n",
        drift.join("\n")
    );
}

/// `is_configured` means *any* rate is set. It decides whether a persisted
/// `cost` object is a real price or the pre-0.19 all-zero "unknown" encoding
/// (which deserializes to `ModelConfig::cost == None`), so `any` vs `all`
/// decides whether a partially-priced config survives a reload.
///
/// Asserted against `CostConfig` values rather than particular presets: pinning
/// a preset as unpriced would forbid a future improvement (pricing DeepSeek is
/// exactly what this file encourages) and would fail with a message describing
/// a regression that did not happen.
///
/// The partial case is the load-bearing one. A preset with no cache-write rate
/// is priced, and only that assertion distinguishes `any` from `all` — an
/// earlier version used two presets whose fields were all-nonzero and all-zero,
/// so it survived flipping the `||` chain to `&&`.
#[test]
fn is_configured_means_any_rate_set() {
    assert!(
        !CostConfig::default().is_configured(),
        "all-zero rates set no rate"
    );

    let no_cache_write = CostConfig::new(5.0, 30.0).with_cache_read(0.5);
    assert!(
        no_cache_write.is_configured(),
        "a provider that charges nothing for cache writes is priced, not unknown"
    );

    let only_one_field = CostConfig::new(0.0, 0.0).with_cache_read(0.1);
    assert!(
        only_one_field.is_configured(),
        "any single rate is enough to count as priced"
    );

    for p in presets() {
        assert!(
            p.cost.is_configured(),
            "{} is in the price audit but reads as unpriced",
            p.constructor
        );
    }
}

/// A request above the tier boundary must cost the tier rate.
///
/// Built from a literal, not a preset, so it keeps testing the mechanism
/// whatever happens to the shipped presets' rates. The boundary is
/// compared against *prompt* tokens — `input + cache_read + cache_write` — so a
/// long reply to a short prompt stays on the base rate.
#[test]
fn cost_usd_applies_the_context_tier_by_prompt_size() {
    use yoagent::provider::{ContextTier, CostConfig};
    use yoagent::types::Usage;

    let cfg = CostConfig::new(5.0, 30.0)
        .with_cache_read(0.5)
        .with_context_tier(ContextTier::new(272_000, 10.0, 45.0).with_cache_read(1.0));

    let usage = |input: u64, output: u64| Usage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        total_tokens: input + output,
    };

    // Below: base rates. 100k in, 1k out => 100k*$5/M + 1k*$30/M.
    let below = cfg.cost_usd(&usage(100_000, 1_000));
    assert!(
        (below - (0.5 + 0.03)).abs() < 1e-9,
        "below the boundary must use base rates, got {below}"
    );

    // Above: tier rates. 300k in, 1k out => 300k*$10/M + 1k*$45/M.
    let above = cfg.cost_usd(&usage(300_000, 1_000));
    assert!(
        (above - (3.0 + 0.045)).abs() < 1e-9,
        "above the boundary must use tier rates, got {above}"
    );

    // Exactly at the boundary is *below* — the field is `above_prompt_tokens`.
    let at = cfg.cost_usd(&usage(272_000, 0));
    assert!(
        (at - 1.36).abs() < 1e-9,
        "the boundary itself must stay on the base rate, got {at}"
    );

    // Cached tokens count toward prompt size: 272k cache reads and 1 fresh
    // token is a 272_001-token prompt, so the tier applies.
    let cached = cfg.cost_usd(&Usage {
        input: 1,
        output: 0,
        cache_read: 272_000,
        cache_write: 0,
        total_tokens: 272_001,
    });
    assert!(
        (cached - (272_000.0 + 10.0) / 1_000_000.0).abs() < 1e-9,
        "cache reads must count toward the boundary and bill at the tier's \
         cache rate, got {cached}"
    );

    // Output does not push a short prompt over the line.
    let long_reply = cfg.cost_usd(&usage(1_000, 400_000));
    assert!(
        (long_reply - (0.005 + 12.0)).abs() < 1e-9,
        "the tier keys on prompt size, not total, got {long_reply}"
    );
}

/// The `cache_write_at_input` allowance, offline: it accepts exactly the
/// band's input rate on an unlisted `cache_write`, and nothing else — so the
/// audit cannot pass vacuously by recording an allowance.
#[test]
fn cache_write_at_input_allowance_is_asserted_both_ways() {
    let preset = |allowance: bool| Preset {
        constructor: "prices.json p/m".into(),
        provider: "p".into(),
        model: "m".into(),
        vendor_page: "https://example.invalid".into(),
        cost: CostConfig::new(5.0, 30.0),
        absent_upstream: None,
        flat_rate_gap: None,
        cache_write_at_input: allowance,
        note: None,
    };
    let with = preset(true);
    let without = preset(false);

    // Positive control: without the allowance, a non-zero unlisted rate is
    // drift — the rule the allowance relaxes.
    assert!(check_absent(&without, "cache_write", 5.0, 5.0).is_some());
    assert!(check_absent(&without, "cache_write", 0.0, 5.0).is_none());
    // With it: the input rate passes, in the base band and in a tier.
    assert!(check_absent(&with, "cache_write", 5.0, 5.0).is_none());
    assert!(check_absent(&with, "tier0.cache_write", 10.0, 10.0).is_none());
    assert!(check_absent(&with, "context_over_200k.cache_write", 10.0, 10.0).is_none());
    // Near-misses: another rate (the base rate in a tier), or zero.
    assert!(check_absent(&with, "cache_write", 6.25, 5.0).is_some());
    assert!(check_absent(&with, "tier0.cache_write", 5.0, 10.0).is_some());
    assert!(check_absent(&with, "cache_write", 0.0, 5.0).is_some());
    // It covers cache_write only.
    assert!(check_absent(&with, "cache_read", 5.0, 5.0).is_some());
    // And goes stale the moment upstream lists the field.
    assert!(stale_allowance(&with, "cache_write").is_some());
    assert!(stale_allowance(&with, "tier0.cache_write").is_some());
    assert!(stale_allowance(&with, "cache_read").is_none());
    assert!(stale_allowance(&without, "cache_write").is_none());
}
