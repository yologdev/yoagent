//! Synthetic but structurally faithful transcripts.
//!
//! `headroom_sweep` grew history with pairs of *user* messages, which is enough
//! to price the headroom policy but invisible to two of the three compaction
//! tiers: level 1 only touches `ToolResult`s and level 2 only summarizes
//! `Assistant` turns. The profiles here emit what the agent loop actually
//! appends — an assistant message carrying tool calls, one `ToolResult` per
//! call (parallel calls included, so orphan handling is exercised), and the
//! occasional user message — so every tier and every boundary rule runs.
//!
//! Sizes come from a seeded PRNG, so a run is fully deterministic.

use yoagent::types::{AgentMessage, Content, Message, StopReason, Usage};

/// SplitMix64: tiny, well-distributed, and stable across Rust releases (unlike
/// `DefaultHasher`), so a seed reproduces the same transcript forever.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next_u64() % (hi - lo + 1) as u64) as usize
    }

    /// Log-uniform in `lo..=hi` — the heavy tail of real command output.
    fn log_range(&mut self, lo: usize, hi: usize) -> usize {
        let u = self.unit();
        ((lo as f64) * ((hi as f64) / (lo as f64)).powf(u)).round() as usize
    }

    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

/// A growth profile: what one loop iteration appends to history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// No tools. A user message and an assistant reply per turn (~0.7K/turn).
    Chat,
    /// #150's live shape (`examples/long_horizon.rs`): one tool call per turn
    /// returning a 60-line record of ~100-char lines (~1.6K/turn).
    Records,
    /// A coding agent: 1–3 parallel calls per turn over `bash` (heavy-tailed
    /// output, 3–3000 lines), `read_file` (40–500 lines, exempt from the line
    /// cap as in the shipped defaults) and small edits (~3K/turn after the cap).
    Coding,
    /// Alternating 10-turn phases of `Chat` and `Coding`.
    Mixed,
}

impl Profile {
    pub const ALL: [Profile; 4] = [
        Profile::Chat,
        Profile::Records,
        Profile::Coding,
        Profile::Mixed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Profile::Chat => "chat",
            Profile::Records => "records (#150)",
            Profile::Coding => "coding (tool-heavy)",
            Profile::Mixed => "mixed",
        }
    }

    fn seed_salt(self) -> u64 {
        match self {
            Profile::Chat => 0x11,
            Profile::Records => 0x22,
            Profile::Coding => 0x33,
            Profile::Mixed => 0x44,
        }
    }
}

/// What one loop iteration appends after the request: the assistant reply,
/// the (untruncated) results of its tool calls, and possibly a user message
/// that arrives before the next request.
pub struct Step {
    pub assistant: AgentMessage,
    pub results: Vec<AgentMessage>,
    pub user: Option<AgentMessage>,
}

pub const TASK_PREFIX: &str = "TASK:";
pub const USER_PREFIX: &str = "USER:";

/// The opening prompt — the thing `keep_first` exists to protect.
pub fn task_prompt() -> AgentMessage {
    AgentMessage::Llm(Message::User {
        content: vec![Content::Text {
            text: format!(
                "{TASK_PREFIX} work through the backlog below; the acceptance criterion is \
                 that every item ends green and the ASSET CODE stays 7Q-4411. {}",
                "Constraint detail. ".repeat(12)
            ),
        }],
        timestamp: 0,
    })
}

fn filler(tokens: usize) -> String {
    // ~4 chars/token, the same estimate `context::estimate_tokens` uses.
    "w".repeat(tokens * 4)
}

fn output(rng: &mut Rng, turn: usize, lines: usize, lo: usize, hi: usize) -> String {
    let mut s = String::new();
    for l in 0..lines {
        if l > 0 {
            s.push('\n');
        }
        let w = rng.range(lo, hi);
        s.push_str(&format!("t{turn} l{l} "));
        s.push_str(&"o".repeat(w));
    }
    s
}

fn user(text: String, turn: usize) -> AgentMessage {
    AgentMessage::Llm(Message::User {
        content: vec![Content::Text { text }],
        timestamp: turn as u64,
    })
}

fn assistant(content: Vec<Content>, turn: usize) -> AgentMessage {
    let stop = if content
        .iter()
        .any(|c| matches!(c, Content::ToolCall { .. }))
    {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    };
    AgentMessage::Llm(
        Message::assistant(content, stop, "sim", "sim", Usage::default())
            .with_timestamp(turn as u64),
    )
}

fn tool_result(id: String, name: &str, text: String, turn: usize) -> AgentMessage {
    AgentMessage::Llm(Message::ToolResult {
        tool_call_id: id,
        tool_name: name.to_string(),
        content: vec![Content::Text { text }],
        is_error: false,
        timestamp: turn as u64,
    })
}

fn call(id: &str, name: &str, args: serde_json::Value) -> Content {
    Content::tool_call(id, name, args)
}

/// Deterministic transcript generator for one session.
pub struct Generator {
    profile: Profile,
    rng: Rng,
}

impl Generator {
    pub fn new(profile: Profile, seed: u64) -> Self {
        Self {
            profile,
            rng: Rng::new(seed.wrapping_mul(1_000_003) ^ profile.seed_salt()),
        }
    }

    /// The response to request `turn` (1-based; the task prompt is turn 0).
    pub fn step(&mut self, turn: usize) -> Step {
        match self.profile {
            Profile::Chat => self.chat(turn),
            Profile::Records => self.records(turn),
            Profile::Coding => self.coding(turn),
            Profile::Mixed => {
                if (turn / 10) % 2 == 0 {
                    self.chat(turn)
                } else {
                    self.coding(turn)
                }
            }
        }
    }

    fn chat(&mut self, turn: usize) -> Step {
        let a = self.rng.range(150, 900);
        let u = self.rng.range(40, 200);
        let reply = filler(a);
        let ask = filler(u);
        Step {
            assistant: assistant(vec![Content::Text { text: reply }], turn),
            results: Vec::new(),
            user: Some(user(format!("{USER_PREFIX} {ask}"), turn)),
        }
    }

    fn records(&mut self, turn: usize) -> Step {
        let id = format!("call-{turn}-0");
        let body = output(&mut self.rng, turn, 60, 88, 108);
        Step {
            assistant: assistant(
                vec![
                    Content::Text {
                        text: format!("Fetching record {turn}."),
                    },
                    call(&id, "fetch_record", serde_json::json!({ "n": turn })),
                ],
                turn,
            ),
            results: vec![tool_result(id, "fetch_record", body, turn)],
            user: (turn % 10 == 0).then(|| {
                user(
                    format!("{USER_PREFIX} continue with the next ten records"),
                    turn,
                )
            }),
        }
    }

    fn coding(&mut self, turn: usize) -> Step {
        let r = self.rng.unit();
        let calls = if r < 0.7 {
            1
        } else if r < 0.9 {
            2
        } else {
            3
        };
        let text_tokens = self.rng.range(20, 150);
        let mut content = vec![Content::Text {
            text: filler(text_tokens),
        }];
        let mut results = Vec::new();
        for k in 0..calls {
            let id = format!("call-{turn}-{k}");
            let pick = self.rng.unit();
            let (name, body, args) = if pick < 0.55 {
                let u = self.rng.unit();
                let lines = if u < 0.6 {
                    self.rng.range(3, 40)
                } else if u < 0.85 {
                    self.rng.range(40, 300)
                } else {
                    self.rng.log_range(300, 3000)
                };
                let body = output(&mut self.rng, turn, lines, 30, 110);
                (
                    "bash",
                    body,
                    serde_json::json!({ "command": format!("cargo test step{turn}") }),
                )
            } else if pick < 0.9 {
                let lines = self.rng.range(40, 500);
                let body = output(&mut self.rng, turn, lines, 20, 90);
                (
                    "read_file",
                    body,
                    serde_json::json!({ "path": format!("src/mod{turn}.rs") }),
                )
            } else {
                let body = output(&mut self.rng, turn, 3, 20, 40);
                (
                    "edit_file",
                    body,
                    serde_json::json!({ "path": format!("src/mod{turn}.rs"), "old": "a", "new": "b" }),
                )
            };
            content.push(call(&id, name, args));
            results.push(tool_result(id, name, body, turn));
        }
        Step {
            assistant: assistant(content, turn),
            results,
            user: self.rng.chance(1.0 / 12.0).then(|| {
                user(
                    format!("{USER_PREFIX} also handle item {turn} before moving on"),
                    turn,
                )
            }),
        }
    }
}
