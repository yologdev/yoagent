// The `yoagent` service as a TypeScript / JavaScript rutis plugin sees it.
//
// A host running yoagent-rutis with its `node` feature provides the service
// `yoagent`. A plugin that injects it registers a handler: an object of async
// functions, passed by reference (rutis calls them in the plugin's own
// process). Python plugins use the same names and shapes, with a class
// instance or a dict of functions as the handler.
//
//   import { definePlugin } from '@arcships/rutis'
//   import type { Yoagent } from './yoagent'
//
//   export default definePlugin({
//     inject: ['yoagent'],
//     apply(ctx) {
//       const yoagent = ctx.use<Yoagent>('yoagent')
//       ctx.effect(yoagent.register('no-shell', {
//         async before_tool(call) {
//           if (call.tool === 'bash') return { deny: 'shell access is disabled' }
//         },
//       }))
//     },
//   })
//
// There is no SDK package yet: copy this file into your plugin.

/** The `yoagent` service. */
export interface Yoagent {
  /**
   * Register a handler. Names are unique across every plugin of the host
   * (Rust, TypeScript and Python alike): a taken name throws.
   *
   * Returns a function that unregisters it: pass it to `ctx.effect`, so the
   * handler goes when the plugin unloads. When the plugin's runtime process
   * exits or crashes, its handlers are removed too.
   *
   * Agents get the handler from their next run on; a run keeps the handlers
   * it started with. A handler that is gone while a run still holds it is
   * unavailable: its tool calls fail, its `before_tool` denies, its
   * `on_input` rejects, its `after_tool` withholds the result.
   */
  register(name: string, handler: Handler, options?: Options): () => void
}

export interface Options {
  /**
   * The event types `on_event` receives (the `type` of yoagent's
   * `AgentEvent`, e.g. `"toolExecutionEnd"`, `"agentEnd"`). Each delivered
   * event is one call into the plugin's process, so only those listed are
   * sent. Required with `on_event`, and only with it.
   */
  events?: AgentEventType[]
}

/**
 * The hooks, every one optional and async. Arguments are plain JSON (copied
 * across processes). A hook that throws, or that does not answer within the
 * host's timeout (60 s policy hooks, 30 s `on_input`, 5 s the others, by
 * default), counts as failed: `before_tool` then denies the call, `on_input`
 * rejects the input, `after_tool` withholds the result, and the others are
 * skipped (or fail the run, if the host made the bridge required).
 *
 * Hooks run in registration order across every plugin: a `deny` wins, an
 * `args` rewrite feeds the next handler, notes are joined.
 */
export interface Handler {
  /** Tools to offer for a run (asked once, at its start). */
  tools?(run: RunInfo): Promise<ToolSpec[]>
  /** Run one of this handler's tools. Required with `tools`. */
  call_tool?(call: ToolCall): Promise<ToolResult>
  /** Judge a tool call: every call of the run, the agent's own tools too. */
  before_tool?(call: ToolCall): Promise<ToolVerdict>
  /** Edit a call's output before the model, the history and consumers see it (redaction). */
  after_tool?(call: ToolCall, output: ToolOutput): Promise<OutputEdit>
  /** Before each model request. */
  before_model?(turn: Turn): Promise<TurnVerdict>
  /** Judge a prompted run's input. */
  on_input?(input: Input): Promise<InputVerdict>
  /** When the model ends its answer (a verifier). */
  on_stop?(stop: Stop): Promise<StopVerdict>
  /** When the run ends, however it ends. */
  finish?(outcome: Outcome): Promise<void>
  /** Each event of the types in `options.events`, in order. Delivered asynchronously: the run never waits for it. */
  on_event?(event: AgentEvent): Promise<void>
}

/** The run a hook is called for. */
export interface RunInfo {
  /** Unique per run. */
  run_id: string
  /** The host's label for the run, if any (informational, not an identity). */
  label: string | null
  /** 0 for a top-level run, 1 for a sub-agent's run, and so on. */
  depth: number
  /** For a delegated run: the tool call that started it, and the calling run. */
  delegated_by: string | null
  parent_run_id: string | null
}

export interface ToolCall extends RunInfo {
  tool: string
  call_id: string
  /** As they stand: rewritten by earlier handlers, if any did. */
  args: Record<string, unknown>
  /** What the user asked for (prose; not a stable format). */
  user_request: string | null
  latest_user_text: string | null
}

/** Allow (nothing), deny with a reason the model sees, or rewrite the arguments. */
export type ToolVerdict = void | null | { deny: string } | { args: Record<string, unknown> }

export interface ToolOutput {
  /** The text blocks, joined. */
  text: string
  /** yoagent's content blocks (text and images). */
  content: unknown[]
  details: unknown
  is_error: boolean
}

/** Keep the output (nothing), or replace parts of it: `text` replaces every content block with one text block. */
export type OutputEdit = void | null | { text?: string; details?: unknown; is_error?: boolean }

export interface Turn extends RunInfo {
  model: string
  user_request: string | null
  latest_user_text: string | null
  /** Names of the tools offered on this request. */
  tools: string[]
}

/** Nothing, a note appended to the request's latest user turn (never stored), or stop the run. */
export type TurnVerdict = void | null | string | { note: string } | { stop: string }

export interface Input extends RunInfo {
  /** Every user text block of the prompts, joined by newlines. */
  text: string
}

export type InputVerdict = void | null | { reject: string }

export interface Stop extends RunInfo {
  /** The answer's text. */
  answer: string
  /** How many times extensions have continued this run already. */
  continues: number
}

/** Accept (nothing), send the model back with a message, or fail the answer. */
export type StopVerdict = void | null | { continue: string } | { fail: string }

export interface Outcome extends RunInfo {
  end: 'completed' | 'stopped' | 'rejected' | 'cancelled' | 'failed' | 'other'
  /** Why it stopped or was rejected. */
  reason?: string
  /** Why it failed, and the extension that failed it, if one did. */
  error?: string
  extension?: string | null
  /** The last assistant message's stop reason (`"stop"`, `"toolUse"`, ...). */
  stop_reason: string | null
}

export interface ToolSpec {
  name: string
  label?: string
  description?: string
  /** JSON Schema of the arguments (default: an object with no properties). */
  parameters?: Record<string, unknown>
}

/** The tool's text, or the text plus details; `is_error` (or a throw) fails the call. */
export type ToolResult = string | { text?: string; details?: unknown; is_error?: boolean }

export type AgentEventType =
  | 'agentStart'
  | 'agentEnd'
  | 'turnStart'
  | 'turnEnd'
  | 'messageStart'
  | 'messageUpdate'
  | 'messageEnd'
  | 'toolExecutionStart'
  | 'toolExecutionUpdate'
  | 'toolExecutionEnd'
  | 'progressMessage'
  | 'inputRejected'
  | 'providerRetry'
  | 'loopDetected'
  | 'contextCompacted'
  | (string & {})

/** yoagent's `AgentEvent` JSON (camelCase fields), plus the run. */
export interface AgentEvent {
  type: AgentEventType
  run: RunInfo
  [field: string]: unknown
}
