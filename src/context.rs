//! Context window management — smart truncation and token counting.
//!
//! The #1 engineering challenge for agents. This module provides:
//! - Token estimation (fast, no external deps)
//! - Smart truncation (keep system prompt + recent, summarize middle)
//! - Max iterations / max tokens budget

use crate::types::*;

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Rough token estimate: ~4 chars per token for English text.
/// Good enough for context budgeting. Use tiktoken-rs for precision.
pub fn estimate_tokens(text: &str) -> usize {
    // Average ~4 chars per token, with overhead for special tokens
    (text.len() + 3) / 4
}

/// Estimate tokens for a single message
pub fn message_tokens(msg: &AgentMessage) -> usize {
    match msg {
        AgentMessage::Llm(m) => match m {
            Message::User { content, .. } => {
                content_tokens(content) + 4 // role overhead
            }
            Message::Assistant { content, .. } => {
                content_tokens(content) + 4
            }
            Message::ToolResult { content, tool_name, .. } => {
                content_tokens(content) + estimate_tokens(tool_name) + 8
            }
        },
        AgentMessage::Extension { data, .. } => {
            estimate_tokens(&data.to_string()) + 4
        }
    }
}

/// Estimate tokens for content blocks
fn content_tokens(content: &[Content]) -> usize {
    content.iter().map(|c| match c {
        Content::Text { text } => estimate_tokens(text),
        Content::Image { .. } => 1000, // Images are ~1k tokens in most APIs
        Content::Thinking { thinking, .. } => estimate_tokens(thinking),
        Content::ToolCall { name, arguments, .. } => {
            estimate_tokens(name) + estimate_tokens(&arguments.to_string()) + 8
        }
    }).sum()
}

/// Estimate total tokens for a message list
pub fn total_tokens(messages: &[AgentMessage]) -> usize {
    messages.iter().map(message_tokens).sum()
}

// ---------------------------------------------------------------------------
// Context truncation strategies
// ---------------------------------------------------------------------------

/// Configuration for context management
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Maximum context tokens (leave room for response)
    pub max_context_tokens: usize,
    /// Tokens reserved for the system prompt
    pub system_prompt_tokens: usize,
    /// Minimum recent messages to always keep
    pub keep_recent: usize,
    /// Minimum first messages to always keep (initial instructions, etc.)
    pub keep_first: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: 100_000,
            system_prompt_tokens: 4_000,
            keep_recent: 10,
            keep_first: 2,
        }
    }
}

/// Truncate messages to fit within token budget.
///
/// Strategy: Keep first N messages + last M messages, drop the middle.
/// Inserts a "[context truncated]" marker where messages were removed.
pub fn truncate_messages(
    messages: Vec<AgentMessage>,
    config: &ContextConfig,
) -> Vec<AgentMessage> {
    let available = config.max_context_tokens.saturating_sub(config.system_prompt_tokens);
    let current = total_tokens(&messages);

    if current <= available {
        return messages;
    }

    let len = messages.len();
    if len <= config.keep_first + config.keep_recent {
        // Too few messages to truncate intelligently — just return as-is
        return messages;
    }

    // Keep first N and last M, try to fit
    let first_end = config.keep_first.min(len);
    let recent_start = len.saturating_sub(config.keep_recent);

    // If first and recent overlap, just keep everything
    if first_end >= recent_start {
        return messages;
    }

    let first_msgs = &messages[..first_end];
    let recent_msgs = &messages[recent_start..];

    let first_tokens: usize = first_msgs.iter().map(message_tokens).sum();
    let recent_tokens: usize = recent_msgs.iter().map(message_tokens).sum();
    let marker_tokens = 20; // "[context truncated: N messages removed]"

    if first_tokens + recent_tokens + marker_tokens <= available {
        // First + recent fit — done
        let mut result = first_msgs.to_vec();
        let removed = recent_start - first_end;
        result.push(AgentMessage::Llm(Message::User {
            content: vec![Content::Text {
                text: format!(
                    "[Context truncated: {} messages removed to fit context window]",
                    removed
                ),
            }],
            timestamp: now_ms(),
        }));
        result.extend_from_slice(recent_msgs);
        return result;
    }

    // Even first + recent don't fit — keep only recent, progressively
    let mut result = Vec::new();
    let mut budget = available;

    // Walk backwards from the end
    for msg in messages.iter().rev() {
        let tokens = message_tokens(msg);
        if tokens > budget {
            break;
        }
        budget -= tokens;
        result.push(msg.clone());
    }

    result.reverse();

    if result.len() < messages.len() {
        let removed = messages.len() - result.len();
        result.insert(
            0,
            AgentMessage::Llm(Message::User {
                content: vec![Content::Text {
                    text: format!(
                        "[Context truncated: {} messages removed to fit context window]",
                        removed
                    ),
                }],
                timestamp: now_ms(),
            }),
        );
    }

    result
}

// ---------------------------------------------------------------------------
// Execution bounds
// ---------------------------------------------------------------------------

/// Execution limits for the agent loop
#[derive(Debug, Clone)]
pub struct ExecutionLimits {
    /// Maximum number of turns (LLM calls)
    pub max_turns: usize,
    /// Maximum total tokens consumed
    pub max_total_tokens: usize,
    /// Maximum wall-clock time
    pub max_duration: std::time::Duration,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_total_tokens: 1_000_000,
            max_duration: std::time::Duration::from_secs(600), // 10 minutes
        }
    }
}

/// Tracks execution state against limits
pub struct ExecutionTracker {
    pub limits: ExecutionLimits,
    pub turns: usize,
    pub tokens_used: usize,
    pub started_at: std::time::Instant,
}

impl ExecutionTracker {
    pub fn new(limits: ExecutionLimits) -> Self {
        Self {
            limits,
            turns: 0,
            tokens_used: 0,
            started_at: std::time::Instant::now(),
        }
    }

    pub fn record_turn(&mut self, tokens: usize) {
        self.turns += 1;
        self.tokens_used += tokens;
    }

    /// Check if any limit has been exceeded. Returns the reason if so.
    pub fn check_limits(&self) -> Option<String> {
        if self.turns >= self.limits.max_turns {
            return Some(format!(
                "Max turns reached ({}/{})",
                self.turns, self.limits.max_turns
            ));
        }
        if self.tokens_used >= self.limits.max_total_tokens {
            return Some(format!(
                "Max tokens reached ({}/{})",
                self.tokens_used, self.limits.max_total_tokens
            ));
        }
        let elapsed = self.started_at.elapsed();
        if elapsed >= self.limits.max_duration {
            return Some(format!(
                "Max duration reached ({:.0}s/{:.0}s)",
                elapsed.as_secs_f64(),
                self.limits.max_duration.as_secs_f64()
            ));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("hello world") > 0);
        assert!(estimate_tokens("hello world") < 10);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_truncate_within_budget() {
        let messages = vec![
            AgentMessage::Llm(Message::user("Hello")),
            AgentMessage::Llm(Message::user("World")),
        ];
        let config = ContextConfig::default();
        let result = truncate_messages(messages.clone(), &config);
        assert_eq!(result.len(), 2); // No truncation needed
    }

    #[test]
    fn test_truncate_drops_middle() {
        // Create many messages that exceed budget
        let mut messages = Vec::new();
        for i in 0..100 {
            messages.push(AgentMessage::Llm(Message::user(
                format!("Message {} with some content to use up tokens: {}", i, "x".repeat(200))
            )));
        }

        let config = ContextConfig {
            max_context_tokens: 500,
            system_prompt_tokens: 100,
            keep_recent: 5,
            keep_first: 2,
            ..Default::default()
        };

        let result = truncate_messages(messages, &config);

        // Should have: first 2 + marker + last 5 = 8, OR fewer if even that doesn't fit
        assert!(result.len() < 100);
        assert!(result.len() >= 2); // At least some messages survive
    }

    #[test]
    fn test_execution_limits() {
        let limits = ExecutionLimits {
            max_turns: 3,
            max_total_tokens: 1000,
            max_duration: std::time::Duration::from_secs(60),
        };

        let mut tracker = ExecutionTracker::new(limits);
        assert!(tracker.check_limits().is_none());

        tracker.record_turn(100);
        tracker.record_turn(100);
        assert!(tracker.check_limits().is_none());

        tracker.record_turn(100);
        assert!(tracker.check_limits().is_some()); // Max turns = 3
    }
}
