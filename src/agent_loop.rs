//! The core agent loop: prompt → LLM stream → tool execution → repeat.
//!
//! This is the heart of yo-agent. It mirrors pi-agent-core's agent-loop.ts:
//!
//! - `agent_loop()` starts with new prompt messages
//! - `agent_loop_continue()` resumes from existing context
//!
//! Both return a stream of `AgentEvent`s.

use crate::context::{self, ContextConfig, ExecutionLimits, ExecutionTracker};
use crate::provider::{StreamConfig, StreamEvent, StreamProvider, ToolDefinition};
use crate::types::*;

/// Type alias for convert_to_llm callback.
pub type ConvertToLlmFn = Box<dyn Fn(&[AgentMessage]) -> Vec<Message> + Send + Sync>;
/// Type alias for transform_context callback.
pub type TransformContextFn = Box<dyn Fn(Vec<AgentMessage>) -> Vec<AgentMessage> + Send + Sync>;
/// Type alias for steering/follow-up message callbacks.
pub type GetMessagesFn = Box<dyn Fn() -> Vec<AgentMessage> + Send + Sync>;
use tokio::sync::mpsc;
use tracing::warn;

/// Configuration for the agent loop
pub struct AgentLoopConfig<'a> {
    pub provider: &'a dyn StreamProvider,
    pub model: String,
    pub api_key: String,
    pub thinking_level: ThinkingLevel,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,

    /// Convert AgentMessage[] → Message[] before each LLM call.
    /// Default: keep only LLM-compatible messages.
    pub convert_to_llm: Option<ConvertToLlmFn>,

    /// Transform context before convert_to_llm (for pruning, compaction).
    pub transform_context: Option<TransformContextFn>,

    /// Get steering messages (user interruptions mid-run).
    pub get_steering_messages: Option<GetMessagesFn>,

    /// Get follow-up messages (queued work after agent finishes).
    pub get_follow_up_messages: Option<GetMessagesFn>,

    /// Context window configuration (auto-compaction).
    pub context_config: Option<ContextConfig>,

    /// Execution limits (max turns, tokens, duration).
    pub execution_limits: Option<ExecutionLimits>,
}

/// Default convert_to_llm: keep only user/assistant/toolResult messages.
fn default_convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| m.as_llm().cloned())
        .collect()
}

/// Start an agent loop with new prompt messages.
pub async fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &AgentLoopConfig<'_>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<AgentMessage> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();

    // Add prompts to context
    for prompt in &prompts {
        context.messages.push(prompt.clone());
    }

    tx.send(AgentEvent::AgentStart).ok();
    tx.send(AgentEvent::TurnStart).ok();

    // Emit events for each prompt message
    for prompt in &prompts {
        tx.send(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .ok();
        tx.send(AgentEvent::MessageEnd {
            message: prompt.clone(),
        })
        .ok();
    }

    run_loop(context, &mut new_messages, config, &tx, &cancel).await;

    tx.send(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .ok();
    new_messages
}

/// Continue an agent loop from existing context (for retries).
pub async fn agent_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig<'_>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<AgentMessage> {
    assert!(
        !context.messages.is_empty(),
        "Cannot continue: no messages in context"
    );

    if let Some(last) = context.messages.last() {
        assert!(
            last.role() != "assistant",
            "Cannot continue from assistant message"
        );
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();

    tx.send(AgentEvent::AgentStart).ok();
    tx.send(AgentEvent::TurnStart).ok();

    run_loop(context, &mut new_messages, config, &tx, &cancel).await;

    tx.send(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .ok();
    new_messages
}

/// Main loop logic shared by agent_loop and agent_loop_continue.
///
/// Outer loop: continues when follow-up messages arrive after agent would stop.
/// Inner loop: process tool calls and steering messages.
async fn run_loop(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig<'_>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) {
    let mut first_turn = true;
    let mut tracker = config
        .execution_limits
        .as_ref()
        .map(|limits| ExecutionTracker::new(limits.clone()));

    // Check for steering messages at start
    let mut pending: Vec<AgentMessage> = config
        .get_steering_messages
        .as_ref()
        .map(|f| f())
        .unwrap_or_default();

    // Outer loop: follow-ups after agent would stop
    loop {
        if cancel.is_cancelled() {
            return;
        }

        let mut steering_after_tools: Option<Vec<AgentMessage>> = None;

        // Inner loop: runs at least once, then continues if tool calls or pending messages
        loop {
            if cancel.is_cancelled() {
                return;
            }

            if !first_turn {
                tx.send(AgentEvent::TurnStart).ok();
            } else {
                first_turn = false;
            }

            // Inject pending messages
            if !pending.is_empty() {
                for msg in pending.drain(..) {
                    tx.send(AgentEvent::MessageStart {
                        message: msg.clone(),
                    })
                    .ok();
                    tx.send(AgentEvent::MessageEnd {
                        message: msg.clone(),
                    })
                    .ok();
                    context.messages.push(msg.clone());
                    new_messages.push(msg);
                }
            }

            // Check execution limits
            if let Some(ref tracker) = tracker {
                if let Some(reason) = tracker.check_limits() {
                    warn!("Execution limit reached: {}", reason);
                    let limit_msg = AgentMessage::Llm(Message::User {
                        content: vec![Content::Text {
                            text: format!("[Agent stopped: {}]", reason),
                        }],
                        timestamp: now_ms(),
                    });
                    tx.send(AgentEvent::MessageStart {
                        message: limit_msg.clone(),
                    })
                    .ok();
                    tx.send(AgentEvent::MessageEnd {
                        message: limit_msg.clone(),
                    })
                    .ok();
                    context.messages.push(limit_msg.clone());
                    new_messages.push(limit_msg);
                    return;
                }
            }

            // Compact context if configured (tiered: tool outputs → summarize → drop)
            if let Some(ref ctx_config) = config.context_config {
                context.messages =
                    context::compact_messages(std::mem::take(&mut context.messages), ctx_config);
            }

            // Stream assistant response
            let message = stream_assistant_response(context, config, tx, cancel).await;

            let agent_msg: AgentMessage = message.clone().into();
            context.messages.push(agent_msg.clone());
            new_messages.push(agent_msg.clone());

            // Check for error/abort
            if let Message::Assistant {
                ref stop_reason, ..
            } = message
            {
                if *stop_reason == StopReason::Error || *stop_reason == StopReason::Aborted {
                    tx.send(AgentEvent::TurnEnd {
                        message: agent_msg,
                        tool_results: vec![],
                    })
                    .ok();
                    return;
                }
            }

            // Extract tool calls
            let tool_calls: Vec<_> = match &message {
                Message::Assistant { content, .. } => content
                    .iter()
                    .filter_map(|c| match c {
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some((id.clone(), name.clone(), arguments.clone())),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            };

            let has_tool_calls = !tool_calls.is_empty();
            let mut tool_results: Vec<Message> = Vec::new();

            if has_tool_calls {
                let execution = execute_tool_calls(
                    &context.tools,
                    &tool_calls,
                    tx,
                    cancel,
                    config.get_steering_messages.as_ref(),
                )
                .await;

                tool_results = execution.tool_results;
                steering_after_tools = execution.steering_messages;

                for result in &tool_results {
                    let am: AgentMessage = result.clone().into();
                    context.messages.push(am.clone());
                    new_messages.push(am);
                }
            }

            // Track turn for execution limits
            if let Some(ref mut tracker) = tracker {
                let turn_tokens = match &message {
                    Message::Assistant { usage, .. } => (usage.input + usage.output) as usize,
                    _ => context::message_tokens(&agent_msg),
                };
                tracker.record_turn(turn_tokens);
            }

            tx.send(AgentEvent::TurnEnd {
                message: agent_msg,
                tool_results,
            })
            .ok();

            // Check steering after turn
            if let Some(steering) = steering_after_tools.take() {
                if !steering.is_empty() {
                    pending = steering;
                    continue;
                }
            }

            pending = config
                .get_steering_messages
                .as_ref()
                .map(|f| f())
                .unwrap_or_default();

            // Exit inner loop if no more tool calls and no pending messages
            if !has_tool_calls && pending.is_empty() {
                break;
            }
        }

        // Agent would stop. Check for follow-ups.
        let follow_ups = config
            .get_follow_up_messages
            .as_ref()
            .map(|f| f())
            .unwrap_or_default();

        if !follow_ups.is_empty() {
            pending = follow_ups;
            continue;
        }

        break;
    }
}

/// Stream an assistant response from the LLM.
async fn stream_assistant_response(
    context: &AgentContext,
    config: &AgentLoopConfig<'_>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Message {
    // Apply context transform
    let messages = if let Some(transform) = &config.transform_context {
        transform(context.messages.clone())
    } else {
        context.messages.clone()
    };

    // Convert to LLM messages
    let convert = config.convert_to_llm.as_ref();
    let llm_messages = match convert {
        Some(f) => f(&messages),
        None => default_convert_to_llm(&messages),
    };

    // Build tool definitions
    let tool_defs: Vec<ToolDefinition> = context
        .tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters_schema(),
        })
        .collect();

    let stream_config = StreamConfig {
        model: config.model.clone(),
        system_prompt: context.system_prompt.clone(),
        messages: llm_messages,
        tools: tool_defs,
        thinking_level: config.thinking_level,
        api_key: config.api_key.clone(),
        max_tokens: config.max_tokens,
        temperature: config.temperature,
    };

    // Stream from provider
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
    let provider_cancel = cancel.clone();

    let provider = config.provider;
    // We need to handle this carefully — spawn the provider stream
    // Run provider inline (avoids lifetime issues with &dyn references)
    let result = provider
        .stream(stream_config, stream_tx, provider_cancel)
        .await;

    // Process any events that were sent
    let mut partial_message: Option<AgentMessage> = None;
    while let Ok(event) = stream_rx.try_recv() {
        match &event {
            StreamEvent::Start => {
                // Will be set when Done arrives
            }
            StreamEvent::TextDelta { delta, .. } => {
                if let Some(ref msg) = partial_message {
                    tx.send(AgentEvent::MessageUpdate {
                        message: msg.clone(),
                        delta: StreamDelta::Text {
                            delta: delta.clone(),
                        },
                    })
                    .ok();
                }
            }
            StreamEvent::ThinkingDelta { delta, .. } => {
                if let Some(ref msg) = partial_message {
                    tx.send(AgentEvent::MessageUpdate {
                        message: msg.clone(),
                        delta: StreamDelta::Thinking {
                            delta: delta.clone(),
                        },
                    })
                    .ok();
                }
            }
            StreamEvent::ToolCallDelta { delta, .. } => {
                if let Some(ref msg) = partial_message {
                    tx.send(AgentEvent::MessageUpdate {
                        message: msg.clone(),
                        delta: StreamDelta::ToolCallDelta {
                            delta: delta.clone(),
                        },
                    })
                    .ok();
                }
            }
            StreamEvent::Done { message } => {
                let am: AgentMessage = message.clone().into();
                partial_message = Some(am.clone());
                tx.send(AgentEvent::MessageStart {
                    message: am.clone(),
                })
                .ok();
                tx.send(AgentEvent::MessageEnd { message: am }).ok();
            }
            StreamEvent::Error { message } => {
                let am: AgentMessage = message.clone().into();
                partial_message = Some(am.clone());
                tx.send(AgentEvent::MessageStart {
                    message: am.clone(),
                })
                .ok();
                tx.send(AgentEvent::MessageEnd { message: am }).ok();
            }
            _ => {}
        }
    }

    match result {
        Ok(msg) => msg,
        Err(e) => {
            warn!("Provider error: {}", e);
            Message::Assistant {
                content: vec![Content::Text {
                    text: String::new(),
                }],
                stop_reason: StopReason::Error,
                model: config.model.clone(),
                provider: "unknown".into(),
                usage: Usage::default(),
                timestamp: now_ms(),
                error_message: Some(e.to_string()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

struct ToolExecutionResult {
    tool_results: Vec<Message>,
    steering_messages: Option<Vec<AgentMessage>>,
}

async fn execute_tool_calls(
    tools: &[Box<dyn AgentTool>],
    tool_calls: &[(String, String, serde_json::Value)],
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    get_steering: Option<&GetMessagesFn>,
) -> ToolExecutionResult {
    let mut results: Vec<Message> = Vec::new();
    let mut steering_messages: Option<Vec<AgentMessage>> = None;

    for (index, (id, name, args)) in tool_calls.iter().enumerate() {
        let tool = tools.iter().find(|t| t.name() == name);

        tx.send(AgentEvent::ToolExecutionStart {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            args: args.clone(),
        })
        .ok();

        let (result, is_error) = match tool {
            Some(tool) => match tool.execute(id, args.clone(), cancel.child_token()).await {
                Ok(r) => (r, false),
                Err(e) => (
                    ToolResult {
                        content: vec![Content::Text {
                            text: e.to_string(),
                        }],
                        details: serde_json::Value::Null,
                    },
                    true,
                ),
            },
            None => (
                ToolResult {
                    content: vec![Content::Text {
                        text: format!("Tool {} not found", name),
                    }],
                    details: serde_json::Value::Null,
                },
                true,
            ),
        };

        tx.send(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: result.clone(),
            is_error,
        })
        .ok();

        let tool_result_msg = Message::ToolResult {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            content: result.content,
            is_error,
            timestamp: now_ms(),
        };

        tx.send(AgentEvent::MessageStart {
            message: tool_result_msg.clone().into(),
        })
        .ok();
        tx.send(AgentEvent::MessageEnd {
            message: tool_result_msg.clone().into(),
        })
        .ok();

        results.push(tool_result_msg);

        // Check for steering — skip remaining tools if user interrupted
        if let Some(get_steering_fn) = get_steering {
            let steering = get_steering_fn();
            if !steering.is_empty() {
                steering_messages = Some(steering);

                // Skip remaining tool calls
                for (skip_id, skip_name, _) in &tool_calls[index + 1..] {
                    let skipped = skip_tool_call(skip_id, skip_name, tx);
                    results.push(skipped);
                }
                break;
            }
        }
    }

    ToolExecutionResult {
        tool_results: results,
        steering_messages,
    }
}

fn skip_tool_call(
    tool_call_id: &str,
    tool_name: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Message {
    let result = ToolResult {
        content: vec![Content::Text {
            text: "Skipped due to queued user message.".into(),
        }],
        details: serde_json::Value::Null,
    };

    tx.send(AgentEvent::ToolExecutionStart {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        args: serde_json::Value::Null,
    })
    .ok();

    tx.send(AgentEvent::ToolExecutionEnd {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        result: result.clone(),
        is_error: true,
    })
    .ok();

    let msg = Message::ToolResult {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        content: result.content,
        is_error: true,
        timestamp: now_ms(),
    };

    tx.send(AgentEvent::MessageStart {
        message: msg.clone().into(),
    })
    .ok();
    tx.send(AgentEvent::MessageEnd {
        message: msg.clone().into(),
    })
    .ok();

    msg
}
