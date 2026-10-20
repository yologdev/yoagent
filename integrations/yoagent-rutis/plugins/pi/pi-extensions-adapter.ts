// pi extensions in yoagent: the tools and tool policies of pi coding-agent
// extensions (https://github.com/earendil-works/pi), offered to yoagent
// agents through the yoagent-rutis bridge.
//
// pi extensions are TypeScript modules written against pi's `ExtensionAPI`.
// This adapter loads them with pi's own loader (so their imports — TypeBox,
// `defineTool`, pi's helpers — resolve to pi's real packages, installed
// here), then maps the part of the API that belongs to an agent loop onto
// one yoagent handler. Load it as a rutis-loader row of a Node runtime whose
// package.json is this directory's, with `yoagent` shared in the loader's
// catalog, and list the extensions in its config (files, or directories with
// an `index.ts` / `index.js`; relative paths resolve against `cwd`):
//
//   { "name": "<this file>", "config": { "extensions": ["./my-ext.ts"], "cwd": "/repo" } }
//
// What maps (following pi 1.1.0's own runner and agent loop):
//   pi.registerTool         a yoagent tool, offered while pi would activate it
//                           (exposure `direct` / `model-only`, `defaultActive`
//                           not false, active under `setActiveTools`; the
//                           first registration of a name wins). Arguments go
//                           through `prepareArguments` and pi's validation
//                           before any policy sees them; `execute` gets the
//                           arguments the policies left, and only those: a
//                           call another handler rewrote afterwards is not
//                           run. A throw or `isError` is an error result;
//                           text and image blocks pass through as they are
//                           (pi's shape is yoagent's).
//                           `ctx.executeTool` calls another pi tool the way pi
//                           does (prepared, validated, judged by `tool_call`
//                           with `parentToolCallId`, results through
//                           `tool_result`; never rejects); yoagent's own tools
//                           are not reachable from it. `ctx.tools` lists the
//                           callable ones. Config `nestedCalls` narrows both
//                           (`own-extension`, `off`).
//                           A tool that overrides one of pi's built-ins
//                           (`read`, `edit`, ...) makes the adapter deny
//                           yoagent's counterpart (`read_file`, ...), so the
//                           model cannot go around it. A tool named exactly
//                           like a yoagent built-in (pi's sandboxed `bash`)
//                           refuses the load unless the host says it left
//                           that built-in out (`withoutBuiltins`).
//   pi.setActiveTools       an allowlist over the tools pi knows (yoagent's
//                           built-ins under pi's names, the extensions'
//                           tools): those outside it are not offered and
//                           their calls are denied; tools pi does not know
//                           (MCP, sub-agents, the host's own) are not
//                           affected, as in pi. A tool registered later that
//                           pi would activate joins it, as pi's registry
//                           refresh does. `getActiveTools` / `getAllTools`
//                           answer from the same view.
//   on("tool_call")         `before_tool`, for every call of the run. yoagent's
//                           built-ins are judged under pi's names (TOOL_NAMES),
//                           their arguments translated both ways and a
//                           relative `path` resolved against `cwd` first, so
//                           the tool acts where the policy looked (built-ins
//                           only). `{ block }` denies (`terminate: true` also
//                           stops the run at its next model request — stricter
//                           than pi, which ends only a batch whose results all
//                           set it); in-place changes to
//                           `event.input` rewrite the arguments (a field the
//                           yoagent tool does not have denies the call); a
//                           handler that throws blocks, as in pi.
//   on("tool_result")       `after_tool`: changes to content, details and
//                           isError are chained; only the fields a handler
//                           set are applied (a replaced content keeps its text
//                           and image blocks). A handler that throws withholds the result
//                           — unlike pi, which skips it: a redaction that
//                           failed must not let the raw output through.
//   on("input")             `on_input`: `handled` rejects the prompt (it never
//                           reaches the agent, as in pi); `transform` and a
//                           handler that throws reject it too (yoagent cannot
//                           rewrite a prompt; failing closed).
//   on("before_agent_start") `before_model`: the handlers run once per run;
//                           text they add around `event.systemPrompt` is a
//                           note on the latest user turn of every request of
//                           the run (yoagent never rewrites the system prompt
//                           — the prompt cache depends on it). A handler that
//                           throws, replaces the prompt or changes
//                           `systemPromptOptions` is skipped with a warning
//                           (its policy does not apply); the others still
//                           count. A returned `message` is dropped, the same
//                           handler's addition kept.
//   on("session_start")     fired once, when the adapter starts; a handler
//                           that throws refuses the load (its extension did
//                           not finish setting up — its policies included).
//   on("session_shutdown")  when it unloads.
//   on("message_end")       checked, not applied: each message of the run is
//   on("context")           passed (a copy) to the `message_end` handlers
//                           once — the assistant message in `before_tool`,
//                           before any of its tools runs; prompts and tool
//                           results in `before_model`; the answer in
//                           `on_stop` — and the messages each request sends
//                           to the `context` handlers in `before_model`. A
//                           handler that only reads is fine; one that would
//                           change what it got (a returned `message` /
//                           `messages`, or an edit in place) stops the
//                           adapter, as a deciding event registered late
//                           does: yoagent cannot apply the change. A throw
//                           is reported and changes nothing, as in pi. Only
//                           when such handlers exist at load is the run's
//                           conversation asked for (`options.messages`):
//                           then one registered later is checked the same
//                           way; without any at load, one registered later
//                           is refused like any deciding event. A check the
//                           hook's timeout or the run's cancel cuts short
//                           does not pass (denied / stopped, run again next
//                           time). "Once" is per message as sent: one that
//                           compaction truncated or summarized is new. The
//                           answer's check comes after it was given: a
//                           rewrite fails the run only for a required bridge,
//                           and refuses every later run.
//
// Observers (OBSERVED_EVENTS: `agent_start` / `agent_end`, `turn_start` /
// `turn_end`, `message_start` / `message_update`, `tool_execution_*`) fire
// from yoagent's event stream when a handler for them exists at load (the
// bridge's `on_event`, subscribed to the yoagent types they need): after the
// fact and in order, on copies, on the adapter's own chain (`on_event` returns
// at once, so a slow observer never meets the bridge's per-event timeout; a
// handler gets OBSERVER_HANDLER_BUDGET_MS before the chain moves on; past
// MAX_OBSERVER_BACKLOG waiting, streaming events are dropped, lifecycle ones
// past twice that, counted and reported per burst; unloading waits up to
// OBSERVER_DRAIN_MS for the queue before `session_shutdown`), a result ignored
// (`turn_end`'s `continue` / `entries` reported once), a throw reported once per run; tool calls under pi's names and argument shapes, in the tool
// events and inside messages; `turn_end` only for a turn that ended with an
// assistant message (the index advances at every turn's end), with generated
// entry ids and an empty context preview. `message_end` (the checked path) is
// not ordered with these. One registered later fires if its types are subscribed.
//
// What does not map: no other event is ever fired. Events that would decide
// or rewrite something yoagent does (DECIDING_EVENTS: `context_with_system`,
// `before_provider_request`, ...; `context` and `message_end` too when
// registered after load) refuse the load unless listed in `allowUnmapped`; the rest (an
// observer registered after load whose types are not subscribed, pi's session
// events, `agent_before_settle`, `agent_settled`), commands, shortcuts, flags, renderers, model
// providers, virtual models and MCP servers are reported (at info: expected,
// not a fault; a load failure with `strict`). One registered after load (in a handler or a
// tool) is reported at the next model request or tool call, and a deciding
// one stops the adapter: that run, and every later call, refused. Warnings
// go to the host's logs (`yoagent.log`, else stderr). `pi.appendEntry` and
// `pi.sendMessage` are recorded in the adapter's in-memory session (readable
// through `ctx.sessionManager`; a displayed message is also logged; a message
// never starts a turn); other runtime actions (`pi.sendUserMessage`,
// `pi.setModel`, ...) throw "not available in yoagent". `pi.exec` runs only
// what the host's `exec` setting allows (default nothing; a refusal is a
// failed command, code 126, empty stdout): each factory gets a `pi` whose
// `exec` is gated, so extensions are imported through pi's loader by a
// generated wrapper and run with its `loadExtensionFromFactory` (`loadGated`;
// every load, whatever `exec` is). Fixed answers — `ctx.isProjectTrusted()`
// (the host's `projectTrusted`, default false), `ctx.model`,
// `ctx.modelRegistry`, `ctx.getContextUsage()`, print-mode dialogs — are
// logged once per question and answer as `[pi] fixed answer: …`. `ctx.hasUI` is false and `ctx.ui` behaves as in
// pi's print mode: `confirm` answers false, `select` and `input` nothing, so
// a policy that would ask the user denies instead. When the host provides a
// `ui` service (yoagent-frontend, experimental) and loads `host-ui.ts` next
// to this adapter, the context is pi's RPC mode instead: `hasUI` true, and
// `select`, `confirm`, `input`, `editor` and `notify` reach the user through
// it (no frontend or no answer: the same safe defaults).

import { randomUUID } from 'node:crypto'
import { isAbsolute, resolve } from 'node:path'
import { definePlugin } from '@arcships/rutis'
import type { Cancellable, ContentBlock, Message, RunInfo, Stop, ToolCall, ToolOutput, ToolResult, ToolSpec, Yoagent } from '../yoagent.d.ts'

export interface Config {
  /** The handler's name in the bridge (unique across the host's plugins). */
  name?: string
  /** pi extensions (files, or directories with an `index.ts` / `index.js`), loaded in order. */
  extensions: string[]
  /** The project directory extensions see as `ctx.cwd`; relative paths resolve against it (default: the runtime's). */
  cwd?: string
  /** yoagent tool name → the pi tool name policies see (merged over TOOL_NAMES). */
  toolNames?: Record<string, string>
  /** yoagent built-ins the host left out, so pi tools of the same name are the ones that run. */
  withoutBuiltins?: string[]
  /** Deciding events (DECIDING_EVENTS) the host accepts going unenforced. */
  allowUnmapped?: string[]
  /** Fail loading when an extension registers anything else that does not map. */
  strict?: boolean
  /**
   * Which tools a pi tool may call through `ctx.executeTool`. Nested calls are judged by pi's
   * `tool_call` handlers only, never by the host's per-call policies, so a host that trusts tools
   * per extension narrows them: `"any"` (default, as pi), `"own-extension"` (only tools whose winning
   * registration is the calling tool's own extension's), `"off"`. A refused call is an error result to the caller.
   */
  nestedCalls?: 'any' | 'own-extension' | 'off'
  /**
   * What `pi.exec(command, args)` may run: `false` or `[]` (default: nothing), `true` (anything,
   * as pi), or the commands allowed, each matched against `command` exactly: `"git"` allows `git`
   * (found on `PATH`), not `/usr/bin/git` or `./git`. Only the command is checked; its arguments
   * and `options.cwd` are the extension's, so allowing a command that can run others (`git`, `sh`,
   * `node`, `npx`) allows those too. A refused call resolves as a failed command (code 126, empty
   * `stdout`, the reason in `stderr`) and is reported at warn, once per extension and command.
   * This gates pi's API only: an extension is code the host runs and may start processes itself.
   */
  exec?: boolean | string[]
  /**
   * Grants for single extensions, over `exec`: an extension file (as in `extensions`, resolved
   * against `cwd`) → its own `true` / `false` / command list. A key naming no loaded extension
   * refuses the load (a typo would otherwise grant nothing, silently).
   */
  execFor?: Record<string, boolean | string[]>
  /** What `ctx.isProjectTrusted()` answers (default `false`; pi's own default is `true`). */
  projectTrusted?: boolean
  /**
   * What `ctx.model` is: the model the agent runs. Default: the run's model id (from yoagent's
   * request), under provider `yoagent`; before the first request, `undefined`.
   */
  model?: ModelSpec
  /**
   * What `ctx.modelRegistry` lists (`getAll`, `getAvailable`, `find`; default: `model`, if
   * set). Read-only: its other methods — credentials, model calls, provider registration —
   * throw "not available in yoagent".
   */
  models?: ModelSpec[]
}

/** A model as the host describes it to pi extensions. */
export interface ModelSpec {
  provider: string
  id: string
  name?: string
  contextWindow?: number
  maxTokens?: number
  reasoning?: boolean
  /** Input kinds (default `["text"]`). */
  input?: ('text' | 'image')[]
}

/** A host's model in pi's `Model` shape (frozen; `api` is `yoagent`, no costs or credentials). */
function piModel(spec: ModelSpec) {
  return Object.freeze({
    id: spec.id,
    name: spec.name ?? spec.id,
    api: 'yoagent',
    provider: spec.provider,
    baseUrl: '',
    input: Object.freeze([...(spec.input ?? ['text'])]),
    cost: Object.freeze({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }),
    reasoning: spec.reasoning ?? false,
    contextWindow: spec.contextWindow ?? 128_000,
    maxTokens: spec.maxTokens ?? 4_096,
  })
}

/** Whether `value` is a usable `ModelSpec`. */
const isModelSpec = (value: unknown): value is ModelSpec => {
  const m = value as ModelSpec
  return (
    !!m &&
    typeof m === 'object' &&
    typeof m.provider === 'string' &&
    !!m.provider &&
    typeof m.id === 'string' &&
    !!m.id &&
    (m.name === undefined || typeof m.name === 'string') &&
    (m.contextWindow === undefined || (Number.isInteger(m.contextWindow) && m.contextWindow > 0)) &&
    (m.maxTokens === undefined || (Number.isInteger(m.maxTokens) && m.maxTokens > 0)) &&
    (m.reasoning === undefined || typeof m.reasoning === 'boolean') &&
    (m.input === undefined || (Array.isArray(m.input) && m.input.every((i) => i === 'text' || i === 'image')))
  )
}

/**
 * A read-only `ModelRegistry` over the host's models: the lookups pi extensions use to see
 * models (all of them chat models; `refresh` does nothing); everything else (credentials, model
 * calls, registering providers) throws, reported once. It cannot be written to.
 */
function readOnlyRegistry(models: ReturnType<typeof piModel>[]) {
  const find = (provider: string, id: string) => models.find((m) => m.provider === provider && m.id === id)
  const ofType = (type: string, provider?: string) =>
    type === 'chat' ? models.filter((m) => provider === undefined || m.provider === provider) : []
  const reads: Record<string, (...args: never[]) => unknown> = {
    getAll: () => [...models],
    getAvailable: () => [...models],
    find,
    findOfType: (type: string, provider: string, id: string) => (type === 'chat' ? find(provider, id) : undefined),
    getModelOfType: (type: string, provider: string, id: string) => (type === 'chat' ? find(provider, id) : undefined),
    getModelsOfType: ofType,
    getAvailableOfType: async (type: string, provider?: string) => ofType(type, provider),
    refresh: async () => ({ aborted: false, errors: new Map() }),
    getError: () => undefined,
    getProviderDisplayName: (provider: string) => provider,
    hasConfiguredAuth: () => false,
    isUsingOAuth: () => false,
    getRegisteredProviderIds: () => [...new Set(models.map((m) => m.provider))],
  }
  const refused = new Set<string>()
  return new Proxy(reads, {
    set: () => false,
    defineProperty: () => false,
    deleteProperty: () => false,
    get: (target, key) => {
      // Not a promise, and `JSON.stringify` sees an object with no own data.
      if (typeof key !== 'string' || key === 'then' || key === 'toJSON') return undefined
      if (key in target) return target[key]
      return () => {
        const why = `ctx.modelRegistry.${key}() is not available in yoagent (no credentials, model calls or providers are handed to extensions)`
        if (!refused.has(key)) {
          refused.add(key)
          report('info', `[pi] ${why}`)
        }
        throw new Error(why)
      }
    },
  })
}

/**
 * yoagent's built-in tools under the names pi's own built-ins have, so a pi
 * policy written for `write` or `bash` judges yoagent's `write_file` and
 * `bash`. Arguments are translated both ways (ARGS).
 */
const TOOL_NAMES: Record<string, string> = {
  bash: 'bash',
  read_file: 'read',
  write_file: 'write',
  edit_file: 'edit',
  search: 'grep',
  list_files: 'find',
}

/** The arguments yoagent's built-ins take: a rewrite to any other field cannot apply. */
const YOAGENT_KEYS = new Map(
  Object.entries({
    bash: ['command'],
    read_file: ['path', 'offset', 'limit'],
    write_file: ['path', 'content'],
    edit_file: ['path', 'old_text', 'new_text'],
    search: ['pattern', 'path', 'include', 'case_sensitive'],
    list_files: ['path', 'pattern', 'max_depth'],
  }).map(([tool, keys]) => [tool, new Set(keys)]),
)

type Args = Record<string, unknown>

/**
 * yoagent ↔ pi argument shapes, where they differ, keyed by the yoagent
 * built-in: only those calls are translated (a pi tool that is itself named
 * `edit` keeps its arguments as they are).
 */
const ARGS = new Map<string, { toPi(args: Args): Args; fromPi(input: Args): Args }>(
  Object.entries({
    edit_file: {
      toPi: ({ old_text, new_text, ...rest }: Args) => ({ ...rest, edits: [{ oldText: old_text, newText: new_text }] }),
      fromPi: ({ edits, ...rest }: Args) => {
        const list = edits as { oldText?: unknown; newText?: unknown }[] | undefined
        if (!Array.isArray(list) || list.length !== 1) {
          throw new Error("a pi extension rewrote `edits` into something other than one edit, which yoagent's edit_file cannot run")
        }
        return { ...rest, old_text: list[0].oldText, new_text: list[0].newText }
      },
    },
    search: {
      toPi: ({ include, case_sensitive, ...rest }: Args) => ({
        ...rest,
        ...(include === undefined ? {} : { glob: include }),
        // yoagent's search is case-insensitive unless asked; pi's grep is case-sensitive unless asked.
        ignoreCase: !(case_sensitive ?? false),
      }),
      fromPi: ({ glob, ignoreCase, ...rest }: Args) => ({
        ...rest,
        ...(glob === undefined ? {} : { include: glob }),
        case_sensitive: ignoreCase === false,
      }),
    },
    list_files: {
      // pi's `find` requires a pattern; yoagent's list_files has an optional one.
      toPi: ({ pattern, ...rest }: Args) => ({ ...rest, pattern: pattern ?? '*' }),
      fromPi: ({ pattern, ...rest }: Args) => (pattern === '*' ? rest : { ...rest, pattern }),
    },
  }),
)

/** What `before_agent_start` handlers see as `event.systemPrompt`. */
const PROMPT_MARK = '\u0000yoagent-system-prompt\u0000'

/** The events this adapter fires. */
const MAPPED_EVENTS = new Set(['tool_call', 'tool_result', 'input', 'before_agent_start', 'session_start', 'session_shutdown'])

/**
 * Unfired events whose handlers would decide or rewrite something yoagent does: they refuse the
 * load. `context` and `message_end` are checked at run time instead when present at load (CHECKED_EVENTS).
 */
const DECIDING_EVENTS = new Set([
  'context',
  'context_with_system',
  'message_end',
  'before_provider_request',
  'before_provider_headers',
])

/** Events whose handlers run on copies, stopping the adapter if one would change what it got. */
const CHECKED_EVENTS = new Set(['context', 'message_end'])

/**
 * pi's observer events, fired from yoagent's event stream (`on_event`):
 * after the fact, so a handler's result changes nothing. Each needs the
 * yoagent event types listed. `message_end` is not here: its handlers run
 * on the checked path (CHECKED_EVENTS), once per message.
 */
const OBSERVED_EVENTS = new Map<string, string[]>([
  ['agent_start', ['agentStart']],
  ['agent_end', ['agentEnd']],
  // Both need both: the turn index advances at every turn's end, as in pi.
  ['turn_start', ['turnStart', 'turnEnd']],
  ['turn_end', ['turnStart', 'turnEnd']],
  ['message_start', ['messageStart']],
  ['message_update', ['messageUpdate']],
  ['tool_execution_start', ['toolExecutionStart']],
  ['tool_execution_update', ['toolExecutionStart', 'toolExecutionUpdate']],
  ['tool_execution_end', ['toolExecutionEnd']],
])

/**
 * Most observer events waiting for slow handlers: past it, streaming events
 * (`message_update`, `tool_execution_update`) are dropped; lifecycle events
 * (starts and ends) only past twice as many, so a tracer's spans close.
 */
const MAX_OBSERVER_BACKLOG = 1000

/** Events whose loss leaves only a gap in a stream, not an open span. */
const STREAMING_EVENTS = new Set(['message_update', 'tool_execution_update'])

/**
 * How long one observer handler may take before the queue moves on without
 * it (it is reported once per run; the handler itself is not stopped).
 */
const OBSERVER_HANDLER_BUDGET_MS = 30_000

/** How long unloading waits for queued observer events before `session_shutdown`. */
const OBSERVER_DRAIN_MS = 5_000

/** pi's names for yoagent's stream deltas (`message_update`'s `assistantMessageEvent.type`). */
const DELTA_TYPES: Record<string, string> = { text: 'text_delta', thinking: 'thinking_delta', toolCallDelta: 'toolcall_delta' }

/** pi's message shape from yoagent's (they match; pi's assistant messages also name their API). */
const piMessage = (message: Message) =>
  structuredClone(message.role === 'assistant' && message.api === undefined ? { ...message, api: 'yoagent' } : message)

/** pi runtime actions with no yoagent counterpart. */
const UNSUPPORTED_ACTIONS = [
  'sendUserMessage',
  'setSessionName',
  'getSessionName',
  'setLabel',
  'getSettings',
  'getCommands',
  'getThinkingLevel',
  'setThinkingLevel',
]

/** Any theme call returns its text unstyled (`theme.fg('dim', text)` → text). */
const PLAIN_THEME = new Proxy({} as Record<string, unknown>, {
  get: (_target, key) =>
    key === 'then' ? undefined : key === 'name' ? 'plain' : (...args: unknown[]) => args[args.length - 1],
})

/** Where diagnostics go: the host's logs through `yoagent.log` once `apply` has the service, else stderr. */
let report = (level: 'error' | 'warn' | 'info' | 'debug', message: string) => {
  console.warn(message)
}

/**
 * A question pi's API asks that this adapter answers with a fixed value: said
 * once per question and answer (per runtime, shared by every adapter in it), at info, under one tag hosts can search for, so a plugin
 * that does nothing can be traced to the answer it got.
 */
const answered = new Set<string>()
function fixedAnswer<T>(question: string, answer: T, shown: string = JSON.stringify(answer) ?? 'undefined'): T {
  // Keyed by the answer too: two adapters in one runtime may answer differently.
  const key = `${question}\0${shown}`
  if (!answered.has(key)) {
    answered.add(key)
    report('info', `[pi] fixed answer: ${question} → ${shown}`)
  }
  return answer
}

/** pi's print-mode UI (its `noOpUIContext`): nothing to show, no one to ask. */
const NO_UI = new Proxy(
  {
    select: async () => fixedAnswer('ctx.ui.select() (no user to ask)', undefined),
    confirm: async () => fixedAnswer('ctx.ui.confirm() (no user to ask)', false),
    input: async () => fixedAnswer('ctx.ui.input() (no user to ask)', undefined),
    editor: async () => fixedAnswer('ctx.ui.editor() (no user to ask)', undefined),
    custom: async () => fixedAnswer('ctx.ui.custom() (no user to ask)', undefined),
    notify: (message: string, level?: string) =>
      report(level === 'error' ? 'error' : level === 'warning' ? 'warn' : 'info', `[pi ${level ?? 'info'}] ${message}`),
    onTerminalInput: () => () => {},
    getEditorText: () => '',
    getAllThemes: () => [],
    getTheme: () => undefined,
    getToolsExpanded: () => false,
    setTheme: () => ({ success: false, error: 'UI not available' }),
    theme: PLAIN_THEME,
  } as Record<string, unknown>,
  {
    // Every other UI call (setStatus, setWidget, ...) does nothing, as in pi;
    // `then` stays undefined so the object is not mistaken for a promise.
    get: (target, key) => (key in target ? target[key as string] : key === 'then' ? undefined : () => undefined),
  },
)

/** The host's `ui` service (yoagent-frontend), when it provides one. */
interface HostUi {
  request(request: Record<string, unknown>, timeoutMs?: number): Promise<unknown>
  withdraw?(key: string): Promise<void>
}

/**
 * pi's RPC-mode UI over the host's `ui` service: dialogs reach whichever
 * frontend answers first; a failure, no frontend or no answer in time gives
 * print mode's answer.
 */
const hostUi = (ui: HostUi, contextSignal?: AbortSignal) => {
  const ask = async (request: Record<string, unknown>, opts?: { timeout?: number; signal?: AbortSignal }) => {
    // When the hook is cancelled (the run stopped, the policy timed out) the
    // question is withdrawn: frontends close its dialog. rutis's JavaScript runtime (as of 0.9) cannot
    // cancel a plugin's call to the host, so the adapter says so by key.
    const signal = opts?.signal ?? contextSignal
    if (signal?.aborted) return undefined
    // Unique across runtimes and restarts: the host withdraws by key.
    const key = `pi-${randomUUID()}`
    const failed = (error: unknown) =>
      report('warn', `[pi] could not withdraw a question (it stays open until its timeout): ${message(error)}`)
    const withdraw = () => {
      // A host without `withdraw`: its own timeout ends the question.
      if (typeof ui.withdraw !== 'function') return
      try {
        Promise.resolve(ui.withdraw(key)).catch(failed)
      } catch (error) {
        failed(error)
      }
    }
    signal?.addEventListener('abort', withdraw, { once: true })
    try {
      return await ui.request({ ...request, key }, opts?.timeout)
    } catch (error) {
      if (!signal?.aborted) report('warn', `[pi] a ui request failed, answering as print mode: ${message(error)}`)
      return undefined
    } finally {
      signal?.removeEventListener('abort', withdraw)
    }
  }
  const text = (value: unknown) => (typeof value === 'string' ? value : undefined)
  return new Proxy(
    {
      ...(NO_UI as Record<string, unknown>),
      select: async (title: unknown, options: unknown[], opts?: { timeout?: number; signal?: AbortSignal }) =>
        text(await ask({ kind: 'select', title: String(title), options: (options ?? []).map(String) }, opts)),
      confirm: async (title: unknown, body?: unknown, opts?: { timeout?: number; signal?: AbortSignal }) =>
        (await ask({ kind: 'confirm', title: String(title), message: String(body ?? '') }, opts)) === true,
      input: async (title: unknown, placeholder?: unknown, opts?: { timeout?: number; signal?: AbortSignal }) =>
        text(await ask({ kind: 'input', title: String(title), placeholder: String(placeholder ?? '') }, opts)),
      editor: async (title: unknown, prefill?: unknown) =>
        text(await ask({ kind: 'input', title: String(title), value: String(prefill ?? '') })),
      notify: (body: string, level?: string) => {
        ;(NO_UI as { notify: (m: string, l?: string) => void }).notify(body, level)
        void ask({ kind: 'notify', message: String(body), level: level ?? 'info' })
      },
    } as Record<string, unknown>,
    {
      get: (target, key) => (key in target ? target[key as string] : key === 'then' ? undefined : () => undefined),
    },
  )
}

interface Block {
  type: string
  text?: string
  data?: string
  mimeType?: string
}

interface PiTool {
  name: string
  label?: string
  description?: string
  promptGuidelines?: string[]
  parameters: Record<string, unknown>
  exposure?: string
  defaultActive?: boolean
  prepareArguments?: (args: unknown) => unknown
  execute(id: string, params: unknown, signal: AbortSignal | undefined, onUpdate: unknown, ctx: unknown): Promise<{
    content?: Block[]
    details?: unknown
    isError?: boolean
  }>
}

interface PiExtension {
  path: string
  handlers: Map<string, ((event: unknown, ctx: unknown) => unknown)[]>
  tools: Map<string, { definition: PiTool }>
  commands: Map<string, unknown>
  flags: Map<string, unknown>
  shortcuts: Map<string, unknown>
  messageRenderers: Map<string, unknown>
  toolRenderers?: unknown[]
  entryRenderers?: Map<string, unknown>
}

interface PiRuntime {
  pendingProviderRegistrations?: { name: string; extensionPath: string }[]
  pendingNativeProviderRegistrations?: { provider: { id: string }; extensionPath: string }[]
  pendingVirtualModelRegistrations?: { definition: { provider: string; id: string }; extensionPath: string }[]
  mcpServers?: { list(): { name: string; extensionPath?: string }[] }
  [action: string]: unknown
}

interface PiApi {
  exec(command: string, args: string[], options?: unknown): Promise<ExecResult>
  [member: string]: unknown
}
type ExecResult = { stdout: string; stderr: string; code: number; killed: boolean }
type Factory = (pi: PiApi) => unknown
type Loaded = {
  extensions: PiExtension[]
  errors: { path: string; error: string }[]
  warnings?: { path: string; warning: string }[]
  runtime: PiRuntime
}

/**
 * pi's loader, by file: the package exports only `discoverAndLoadExtensions`,
 * which also loads `<cwd>/.pi/extensions` and `~/.pi/agent/extensions`.
 */
async function piModule<T>(path: string): Promise<T> {
  const index = import.meta.resolve('@earendil-works/pi-coding-agent')
  return import(new URL(path, index).href)
}

/** The event a wrapper below hands its extension's factory over on (the load's own pi event bus). */
const FACTORY_EVENT = 'yoagent-rutis.pi.factory'

/**
 * Load extensions as pi's `loadExtensions` does, except that each factory is
 * given `gate(pi)` instead of pi's API object: `pi.exec` lives on that object,
 * created inside pi's loader with no hook, so gating it means calling the
 * factory ourselves. pi's loader still imports every module (its resolution
 * and aliases): it loads one generated wrapper per extension, which only hands
 * the extension's factory over; pi's exported `loadExtensionFromFactory` then
 * runs it, under the extension's own resolved path.
 */
async function loadGated(paths: string[], cwd: string, gate: (pi: PiApi, path: string) => PiApi): Promise<Loaded> {
  const loader = await piModule<{
    loadExtensions(paths: string[], cwd: string, eventBus: unknown, runtime: unknown): Promise<Loaded>
    loadExtensionFromFactory(factory: Factory, cwd: string, eventBus: unknown, runtime: unknown, path: string): Promise<PiExtension>
    createExtensionRuntime(): PiRuntime
  }>('./core/extensions/loader.js')
  const { createEventBus } = await piModule<{
    createEventBus(): { on(channel: string, handler: (data: unknown) => void): () => void }
  }>('./core/event-bus.js')
  const { resolvePath } = await piModule<{ resolvePath(input: string, base: string, options?: unknown): string }>(
    './utils/paths.js',
  )
  const eventBus = createEventBus()
  const runtime = loader.createExtensionRuntime()
  const extensions: PiExtension[] = []
  const errors: { path: string; error: string }[] = []
  // First every module, through pi's loader (file I/O); then every factory,
  // back to back, as pi runs them: nothing but the factories happens between
  // the last of them and the adapter's load-time checks.
  const imported: { path: string; resolved: string; factory: Factory }[] = []
  const dir = await wrapperDir()
  const wrappers: { wrapper: string; path: string; resolved: string }[] = []
  for (const path of paths) {
    const resolved = resolvePath(path, cwd, { normalizeUnicodeSpaces: true })
    wrappers.push({ wrapper: await writeWrapper(dir, resolved), path, resolved })
  }
  // Each wrapper hands its factory over on this load's own event bus, which
  // calls the handler synchronously: no state shared with other loads, so two
  // adapters loading one file at once each get their own module's factory.
  // pi's loader runs the wrappers in order, one each (a file listed twice runs
  // twice, as in pi).
  const handed: { path: string; factory: unknown }[] = []
  const unsubscribe = eventBus.on(FACTORY_EVENT, (data) => {
    handed.push(data as { path: string; factory: unknown })
  })
  let first: Loaded
  try {
    first = await loader.loadExtensions(
      wrappers.map((w) => w.wrapper),
      cwd,
      eventBus,
      runtime,
    )
  } finally {
    unsubscribe()
  }
  // pi reports failures in the order of the paths it was given.
  const failures = [...first.errors]
  for (const { wrapper, path, resolved } of wrappers) {
    if (failures[0]?.path === wrapper) {
      errors.push({ path, error: failures.shift()!.error.replaceAll(wrapper, resolved) })
      continue
    }
    const next = handed.findIndex((h) => h.path === resolved)
    const factory = next === -1 ? undefined : handed.splice(next, 1)[0].factory
    if (typeof factory !== 'function') {
      errors.push({ path, error: `Extension does not export a valid factory function: ${path}` })
    } else imported.push({ path, resolved, factory: factory as Factory })
  }
  for (const { path, resolved, factory } of imported) {
    try {
      extensions.push(
        await loader.loadExtensionFromFactory((pi) => factory(gate(pi, resolved)), cwd, eventBus, runtime, resolved),
      )
    } catch (error) {
      errors.push({ path, error: `Failed to load extension: ${message(error)}` })
    }
  }
  const warnings = (first.warnings ?? []).map((w) => {
    const at = wrappers.find((x) => x.wrapper === w.path)
    return at ? { path: at.path, warning: w.warning.replaceAll(at.wrapper, at.resolved) } : w
  })
  return { extensions, errors, warnings, runtime }
}

/**
 * The directory the wrappers live in: one per user under the temp directory,
 * kept, so each extension's wrapper has a stable name and content and pi's
 * jiti reuses its transpile cache (a fresh name per load would leave a cache
 * file behind every time). Refused unless it is a real directory of this
 * user's that nobody else can write: a wrapper is code this runtime runs.
 */
async function wrapperDir(): Promise<string> {
  const { mkdir, lstat } = await import('node:fs/promises')
  const { tmpdir } = await import('node:os')
  const { join } = await import('node:path')
  const uid = process.getuid?.()
  const dir = join(tmpdir(), `yoagent-pi-wrappers-${uid ?? 'user'}`)
  try {
    await mkdir(dir, { recursive: true, mode: 0o700 })
    const info = await lstat(dir)
    // Windows reports no POSIX owner or mode bits (a writable directory reads 0o777), and its
    // temp directory is per-user already: there, only that it is a real directory is checked.
    const posix = process.platform !== 'win32'
    if (!info.isDirectory() || (posix && ((uid !== undefined && info.uid !== uid) || (info.mode & 0o022) !== 0))) {
      throw new Error('it is not a directory of this user\'s that only this user can write')
    }
  } catch (error) {
    throw new Error(`pi adapter: cannot use ${dir} for the extension wrappers (every extension is loaded through one there): ${message(error)}`)
  }
  return dir
}

/** Writes (atomically) and returns the wrapper that hands `resolved`'s factory over. */
async function writeWrapper(dir: string, resolved: string): Promise<string> {
  const { writeFile, rename } = await import('node:fs/promises')
  const { join } = await import('node:path')
  const { createHash } = await import('node:crypto')
  const wrapper = join(dir, `${createHash('sha256').update(resolved).digest('hex').slice(0, 32)}.ts`)
  const source =
    `import factory from ${JSON.stringify(resolved)}\n` +
    `export default (pi) => { pi.events.emit(${JSON.stringify(FACTORY_EVENT)}, { path: ${JSON.stringify(resolved)}, factory }) }\n`
  const temporary = `${wrapper}.${randomUUID()}`
  await writeFile(temporary, source, { mode: 0o600 })
  await rename(temporary, wrapper)
  return wrapper
}

/**
 * `pi.exec` as the host's grant for the extension at `path` allows it (`grantFor`: its own in
 * `execFor`, else the row-wide `exec`); everything else on pi's API as it is.
 */
function gateExec(grantFor: (path: string) => boolean | string[]) {
  const refusedOnce = new Set<string>()
  return (pi: PiApi, path: string): PiApi => {
    const exec = grantFor(path)
    if (exec === true) return pi
    const allowed = (command: string) => Array.isArray(exec) && exec.includes(command)
    const gated = (command: string, args: string[], options?: unknown): Promise<ExecResult> => {
      if (allowed(command)) return pi.exec(command, args, options)
      const why = `pi.exec("${command}") is not allowed by the host (the adapter's \`exec\` / \`execFor\` setting)`
      const key = `${path}\0${command}`
      if (!refusedOnce.has(key)) {
        refusedOnce.add(key)
        report('warn', `[pi] ${path}: refused ${why}`)
      }
      return Promise.resolve({ stdout: '', stderr: why, code: 126, killed: false })
    }
    // A copy, not a Proxy: a Proxy's target still hands the real `exec` out
    // through `Object.getOwnPropertyDescriptor`. pi's API methods are
    // closures that never use `this`, and `events` is shared by reference.
    return { ...pi, exec: gated }
  }
}

const short = (path: string) => path.split('/').pop() ?? path

function text(content: Block[] | undefined): string {
  return (content ?? []).map((block) => (block.type === 'text' ? (block.text ?? '') : `[${block.type} block]`)).join('\n')
}

/**
 * Most base64 one result may carry in images: rutis (0.9) closes the channel
 * on a message over 16 MiB, which would end this runtime and every plugin in it.
 */
const MAX_RESULT_IMAGE_CHARS = 12 * 1024 * 1024

/**
 * pi's content blocks as yoagent's (the same shape for text and images); any other block is named in text,
 * and images past MAX_RESULT_IMAGE_CHARS together are left out, named.
 */
function blocks(content: Block[] | undefined): ContentBlock[] {
  let imageChars = 0
  return (content ?? []).map((block): ContentBlock => {
    if (block.type === 'text') return { type: 'text', text: block.text ?? '' }
    if (block.type === 'image' && block.data && block.mimeType) {
      if (imageChars + block.data.length > MAX_RESULT_IMAGE_CHARS) {
        return { type: 'text', text: `[${block.mimeType}: left out, the result's images are too large together]` }
      }
      imageChars += block.data.length
      return { type: 'image', data: block.data, mimeType: block.mimeType }
    }
    return { type: 'text', text: `[${block.type} block]` }
  })
}

/**
 * A message as `message_end`'s seen-set keys it: each image as its type and
 * size, the same whether it came whole or — over the bridge's message limit —
 * as the host's placeholder text, so a message is checked once either way.
 */
function seenAs(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(seenAs)
  if (!value || typeof value !== 'object') return value
  const block = value as Record<string, unknown>
  if (block.type === 'image' && typeof block.data === 'string') {
    const mb = ((Math.floor(block.data.length / 4) * 3) / (1024 * 1024)).toFixed(1)
    return { image: `${block.mimeType ?? 'image'}, ${mb} MB` }
  }
  if (block.type === 'text' && typeof block.text === 'string') {
    const placeholder = /^\[(.+), (\d+\.\d) MB: omitted, too large to send to a plugin\]$/.exec(block.text)
    if (placeholder) return { image: `${placeholder[1]}, ${placeholder[2]} MB` }
  }
  return Object.fromEntries(Object.entries(block).map(([k, v]) => [k, seenAs(v)]))
}

/**
 * JSON with object keys sorted and non-integers at 15 significant digits:
 * arguments cross serde_json, which may reorder keys and parse a float's
 * shortest form one unit off. A big integer (rutis 0.11 sends one beyond
 * ±(2^53−1) as a `bigint`, which `JSON.stringify` refuses) is written as its
 * digits plus `n`, so it never equals a string or a number.
 */
function canon(value: unknown): string {
  return JSON.stringify(value, (_key, v) => {
    if (typeof v === 'bigint') return `${v}n`
    if (typeof v === 'number' && !Number.isInteger(v)) return Number(v.toPrecision(15))
    return v && typeof v === 'object' && !Array.isArray(v)
      ? Object.fromEntries(Object.entries(v as Args).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)))
      : v
  })
}

const message = (error: unknown) => {
  try {
    return error instanceof Error ? error.message : String(error)
  } catch {
    // An error whose own `toString` throws (or that has none).
    return Object.prototype.toString.call(error)
  }
}

export default definePlugin<Config>({
  inject: ['yoagent'],
  config: {
    type: 'object',
    required: ['extensions'],
    properties: {
      name: { type: 'string' },
      extensions: { type: 'array', items: { type: 'string' } },
      cwd: { type: 'string' },
      toolNames: { type: 'object', additionalProperties: { type: 'string' } },
      withoutBuiltins: { type: 'array', items: { type: 'string' } },
      allowUnmapped: { type: 'array', items: { type: 'string' } },
      strict: { type: 'boolean' },
      nestedCalls: { type: 'string', enum: ['any', 'own-extension', 'off'] },
      exec: { anyOf: [{ type: 'boolean' }, { type: 'array', items: { type: 'string' } }] },
      execFor: {
        type: 'object',
        additionalProperties: { anyOf: [{ type: 'boolean' }, { type: 'array', items: { type: 'string' } }] },
      },
      projectTrusted: { type: 'boolean' },
      model: { type: 'object' },
      models: { type: 'array', items: { type: 'object' } },
    },
  },
  async apply(ctx, config) {
    const yoagent = ctx.use<Yoagent>('yoagent')
    // rutis gives every service method a stand-in, one that throws when the
    // host lacks it: so `log` is called, and a throw or a rejection falls back.
    report = (level, message) => {
      try {
        yoagent.log?.(level, message)?.catch(() => console.warn(message))
      } catch {
        console.warn(message)
      }
    }
    const cwd = config.cwd ?? process.cwd()
    const names = new Map(Object.entries({ ...TOOL_NAMES, ...config.toolNames }))
    const withoutBuiltins = new Set(config.withoutBuiltins ?? [])
    const allowUnmapped = new Set(config.allowUnmapped ?? [])
    const nestedCalls = config.nestedCalls ?? 'any'
    if (!['any', 'own-extension', 'off'].includes(nestedCalls)) {
      throw new Error(`nestedCalls must be "any", "own-extension" or "off", not ${JSON.stringify(nestedCalls)}`)
    }

    const { SessionManager } = await import('@earendil-works/pi-coding-agent')
    const { validateToolArguments } = await import('@earendil-works/pi-ai')
    const isGrant = (g: unknown): g is boolean | string[] =>
      typeof g === 'boolean' || (Array.isArray(g) && g.every((c) => typeof c === 'string'))
    const exec = config.exec ?? false
    if (!isGrant(exec)) throw new Error(`exec must be true, false or a list of commands, not ${JSON.stringify(exec)}`)
    const shown = (g: boolean | string[]) => (g === true ? 'anything' : g === false || g.length === 0 ? 'nothing' : g.join(', '))
    report('debug', `[pi] pi.exec allowed: ${shown(exec)}`)
    // Per-extension grants, keyed by the resolved file as `loadGated` resolves `extensions`.
    const { resolvePath } = await piModule<{ resolvePath(input: string, base: string, options?: unknown): string }>(
      './utils/paths.js',
    )
    const resolveExt = (path: string) => resolvePath(path, cwd, { normalizeUnicodeSpaces: true })
    const execFor = new Map<string, boolean | string[]>()
    for (const [path, grant] of Object.entries(config.execFor ?? {})) {
      if (!isGrant(grant)) {
        throw new Error(`execFor["${path}"] must be true, false or a list of commands, not ${JSON.stringify(grant)}`)
      }
      execFor.set(resolveExt(path), grant)
      report('debug', `[pi] pi.exec allowed for ${path}: ${shown(grant)}`)
    }
    const listed = new Set(config.extensions.map(resolveExt))
    const unknown = [...execFor.keys()].filter((path) => !listed.has(path))
    if (unknown.length > 0) {
      throw new Error(`execFor names extensions that are not loaded: ${unknown.join(', ')}`)
    }
    // Models extensions see (`ctx.model`, `ctx.modelRegistry`).
    for (const [what, spec] of [['model', config.model] as const, ...(config.models ?? []).map((m, i) => [`models[${i}]`, m] as const)]) {
      if (spec !== undefined && !isModelSpec(spec)) {
        throw new Error(`${what} must be {provider, id, name?, contextWindow?, maxTokens?, reasoning?, input?}, not ${JSON.stringify(spec)}`)
      }
    }
    const hostModel = config.model ? piModel(config.model) : undefined
    const registry = readOnlyRegistry(config.models ? config.models.map(piModel) : hostModel ? [hostModel] : [])
    /** Per run, the id of the model its latest request used (when the host names no `model`). */
    const runModels = new Map<string, string>()
    /** The latest request's model of any run: for contexts outside a run (`session_start`, ...). */
    let latestModel: string | undefined
    const loaded = await loadGated(config.extensions, cwd, gateExec((path) => execFor.get(path) ?? exec))
    if (loaded.errors.length > 0) {
      throw new Error(`pi extensions failed to load: ${loaded.errors.map((e) => `${e.path}: ${e.error}`).join('; ')}`)
    }
    for (const w of loaded.warnings ?? []) report('warn', `[pi] ${short(w.path)}: ${w.warning}`)
    const extensions = loaded.extensions
    const runtime = loaded.runtime
    // What each extension registered, for hosts that want to know (debug).
    for (const ext of extensions) {
      const list = (keys: Iterable<string>) => [...keys].join(', ') || 'none'
      report('debug', `[pi] loaded ${short(ext.path)}: events ${list(ext.handlers.keys())}; tools ${list(ext.tools.keys())}`)
    }

    /**
     * Set when something that would have to be enforced turns up after load:
     * from then on every tool call is denied, every prompt rejected, every
     * run stopped.
     */
    let refusal: string | undefined

    const sessionManager = SessionManager.inMemory(cwd)
    // A host `ui` service, re-provided in this runtime as `pi-host-ui` by
    // `host-ui.ts`, makes this pi's RPC mode. It is looked up when a context
    // is made, never injected: rutis starts a plugin only once every injected
    // service exists, and hosts without one keep print mode.
    let uiMissingReported = false
    const lookupUi = () => {
      try {
        return ctx.use<HostUi>('pi-host-ui')
      } catch (error) {
        // The usual case without a frontend: print mode. Said once, at debug.
        if (!uiMissingReported) {
          uiMissingReported = true
          report('debug', `[pi] no host ui (print mode): ${message(error)}`)
        }
        return undefined
      }
    }
    const context = (signal?: AbortSignal, runId?: string, systemPrompt = '') => {
      const hostUiService = lookupUi()
      return {
      ui: hostUiService ? hostUi(hostUiService, signal) : NO_UI,
      mode: hostUiService ? 'rpc' : 'print',
      hasUI: hostUiService !== undefined,
      cwd,
      sessionManager,
      get modelRegistry() {
        return fixedAnswer('ctx.modelRegistry', registry, `a read-only list of ${registry.getAll().length} model(s)`)
      },
      get model() {
        const id = (runId !== undefined ? runModels.get(runId) : undefined) ?? latestModel
        const model = hostModel ?? (id ? piModel({ provider: 'yoagent', id }) : undefined)
        return fixedAnswer('ctx.model', model, model ? `${model.provider}/${model.id}` : 'undefined')
      },
      scopedModels: [],
      signal,
      isIdle: () => signal === undefined,
      // pi's own default is true; this adapter loads only the files it is given, never a project's
      // `.pi`, and a host opts in (`projectTrusted`) to what extensions do in a trusted project.
      isProjectTrusted: () => fixedAnswer('ctx.isProjectTrusted()', config.projectTrusted ?? false),
      hasPendingMessages: () => false,
      getContextUsage: () => fixedAnswer('ctx.getContextUsage()', undefined),
      getSystemPrompt: () => systemPrompt,
      abort: () => {
        throw new Error('ctx.abort() is not available in yoagent')
      },
      shutdown: () => {
        throw new Error('ctx.shutdown() is not available in yoagent')
      },
      compact: () => {
        throw new Error('ctx.compact() is not available in yoagent')
      },
      }
    }
    /**
     * The pi tools `ctx.executeTool` can call: pi's callable exposures
     * (`direct` while available, `codemode`, `deferred`; never `model-only`
     * or `hidden`). yoagent's own tools are not reachable from here.
     */
    const callable = () => {
      const offered = available()
      return new Map(
        [...registered()].filter(([name, tool]) => {
          const exposure = tool.exposure ?? 'direct'
          return exposure === 'direct' ? offered.has(name) : exposure === 'codemode' || exposure === 'deferred'
        }),
      )
    }

    type Outcome = {
      toolCall: { id: string; name: string; arguments: unknown }
      result: { content: Block[]; details: unknown }
      isError: boolean
      durationMs?: number
    }

    /**
     * A nested call, as pi's `executeTool` makes it: prepared and validated,
     * judged by the `tool_call` handlers (with `parentToolCallId`), run, and
     * passed through the `tool_result` handlers. Never rejects: an unknown
     * tool, a validation error, a block or a throw is `isError`.
     */
    const executeNested = async (
      runId: string,
      parentId: string,
      id: string,
      name: string,
      args: unknown,
      signal: AbortSignal,
      depth: number,
      caller: string | undefined,
    ): Promise<Outcome> => {
      const failed = (text: string, input: unknown = args): Outcome => ({
        toolCall: { id, name, arguments: input },
        result: { content: [{ type: 'text', text }], details: {} },
        isError: true,
      })
      checkUnmapped(false)
      checkShadowing(false)
      if (refusal) return failed(refusal)
      if (depth >= 8) return failed('nested tool calls are limited to 8 levels')
      // A tool still running after its run ended (a timer, a detached promise).
      if (!snapshots.has(runId)) return failed(`run ${runId} has ended; nested tool calls are no longer possible`)
      // Refused before any lookup or foreign code: the host's scope, reported to the host, since
      // nested calls never reach its per-call policies.
      const refused =
        nestedCalls === 'off' || callableIn(runId).has(name) ? nestedRefusal(runId, caller, name) : undefined
      if (refused) {
        report('info', `[pi] nested call ${parentId} -> ${name} refused: ${refused}`)
        return failed(refused)
      }
      // From the run's snapshot, like a top-level call: a name registered since never takes over.
      const tool = callableIn(runId).get(name)
      if (!tool) return failed(`Tool ${name} not found (yoagent's own tools cannot be called from a pi tool)`)
      let input: Args
      try {
        // As pi: missing arguments are an empty object.
        const copy = structuredClone(args ?? {})
        const prepared = tool.prepareArguments ? tool.prepareArguments(copy) : copy
        input = validateToolArguments(tool as never, { name, arguments: prepared } as never) as Args
      } catch (error) {
        return failed(message(error))
      }
      for (const { ext, fn } of handlers('tool_call')) {
        const event = { type: 'tool_call', toolCallId: id, parentToolCallId: parentId, toolName: name, input }
        let result: { block?: boolean; reason?: string; terminate?: boolean } | undefined
        try {
          result = (await fn(event, context(signal, runId))) as typeof result
        } catch (error) {
          return failed(`pi extension ${short(ext.path)} failed: ${message(error)}`, input)
        }
        if (result?.block) {
          const reason = result.reason ?? `blocked by pi extension ${short(ext.path)}`
          if (result.terminate === true) terminated.set(runId, reason)
          return failed(reason, input)
        }
      }
      const started = performance.now()
      let out: { content?: Block[]; details?: unknown; isError?: boolean }
      try {
        out = await tool.execute(id, input, signal, undefined, toolContext(signal, runId, id, depth + 1, sourceIn(runId, name)))
      } catch (error) {
        // As pi: a throw is an error result, and it still goes through the tool_result handlers.
        out = { content: [{ type: 'text', text: message(error) }], details: {}, isError: true }
      }
      const durationMs = Math.round(performance.now() - started)
      const event = {
        type: 'tool_result',
        toolCallId: id,
        parentToolCallId: parentId,
        toolName: name,
        input,
        content: out.content ?? [],
        details: out.details,
        isError: out.isError === true,
      }
      for (const { ext, fn } of handlers('tool_result')) {
        try {
          const edit = (await fn(event, context(signal, runId))) as { content?: Block[]; details?: unknown; isError?: boolean } | undefined
          if (edit?.content !== undefined) event.content = edit.content
          if (edit?.details !== undefined) event.details = edit.details
          if (edit?.isError !== undefined) event.isError = edit.isError
        } catch (error) {
          // As at the top level: a failed redaction must not let the raw output through.
          return failed(`pi tool_result handler ${short(ext.path)} failed, the result is withheld: ${message(error)}`, input)
        }
      }
      return {
        toolCall: { id, name, arguments: input },
        result: { content: event.content, details: event.details },
        isError: event.isError,
        durationMs,
      }
    }

    /** A tool's `ctx`: the extension context, plus `tools` and `executeTool` for nested calls. */
    const toolContext = (signal: AbortSignal, runId: string, callId: string, depth: number, caller: string | undefined) => {
      let nested = 0
      // Added onto the context, not spread: a spread reads every getter, which would log
      // fixed answers (`ctx.model`, ...) no extension asked for.
      return Object.defineProperties(context(signal, runId), Object.getOwnPropertyDescriptors({
        get tools() {
          // The run's frozen copies (a tool cannot swap another's `execute` through them), only
          // those `nestedCalls` lets this tool call.
          return [...callableIn(runId)].filter(([name]) => !nestedRefusal(runId, caller, name)).map(([, tool]) => tool)
        },
        executeTool: (name: string, args: unknown, options?: { signal?: AbortSignal }) =>
          executeNested(runId, callId, `${callId}/${++nested}`, name, args, options?.signal ?? signal, depth, caller),
      }))
    }

    /** Every handler of `event`, in extension load and registration order. */
    const handlers = (event: string) =>
      extensions.flatMap((ext) => (ext.handlers.get(event) ?? []).map((fn) => ({ ext, fn })))

    /** Every registered tool, the first registration of a name winning (pi's `getAllRegisteredTools`). */
    const registered = () => {
      const byName = new Map<string, PiTool>()
      for (const ext of extensions) {
        for (const { definition } of ext.tools.values()) {
          if (!byName.has(definition.name)) byName.set(definition.name, definition)
        }
      }
      return byName
    }
    /**
     * The extension file that registered each tool `registered()` keeps (first wins, the same
     * rule): pi's resolved path, not the configured spelling (`./x.ts`, `~/x.ts`).
     */
    const registeredBy = () => {
      const paths = new Map<string, string>()
      for (const ext of extensions) {
        for (const { definition } of ext.tools.values()) {
          if (!paths.has(definition.name)) paths.set(definition.name, ext.resolvedPath ?? ext.path)
        }
      }
      return paths
    }

    // pi.setActiveTools: an allowlist over the tools pi knows — yoagent's built-ins under pi's
    // names and the extensions' tools. Other tools (MCP, sub-agents, the host's own) are not pi's
    // to activate and stay outside it, as in pi.
    let active: Set<string> | undefined
    /** Tool names registered when the allowlist was set; one registered later joins it as pi's registry refresh does. */
    let knownAtActivation = new Set<string>()
    const activatedOnRegistration = (tool: PiTool) => {
      const exposure = tool.exposure ?? 'direct'
      return (exposure === 'direct' || exposure === 'model-only') && tool.defaultActive !== false
    }
    /** The tools pi would offer (its `_applyToolLoadout`): activated on registration, or named in the allowlist and not hidden. */
    const available = () => {
      const tools = registered()
      if (active) {
        for (const [name, tool] of tools) {
          if (!knownAtActivation.has(name) && activatedOnRegistration(tool)) active.add(name)
          knownAtActivation.add(name)
        }
      }
      return new Map(
        [...tools].filter(([name, tool]) =>
          active ? active.has(name) && (tool.exposure ?? 'direct') !== 'hidden' : activatedOnRegistration(tool),
        ),
      )
    }
    /** Whether a call to `toolName` (a pi name) is outside the allowlist. */
    const inactive = (toolName: string) =>
      active !== undefined &&
      !active.has(toolName) &&
      (registered().has(toolName) || [...names.values()].includes(toolName))

    // The runtime actions: tool activation is pi's to decide here; the rest has no counterpart.
    runtime.setActiveTools = (toolNames: string[]) => {
      active = new Set(toolNames)
      knownAtActivation = new Set(registered().keys())
    }
    runtime.getActiveTools = () => {
      // Brings in tools registered since setActiveTools, as pi's registry refresh does on registerTool.
      const tools = available()
      if (active) return [...active]
      const builtins = [...names].filter(([yo]) => !withoutBuiltins.has(yo)).map(([, pi]) => pi)
      return [...new Set([...builtins, ...tools.keys()])]
    }
    runtime.getAllTools = () => {
      const tools = registered()
      return [
        // yoagent's built-ins as pi's (an extension's tool of the same name takes its place; one
        // the host left out is not listed).
        ...[...names]
          .filter(([yo, pi]) => !withoutBuiltins.has(yo) && !tools.has(pi))
          .map(([yo, pi]) => ({
            name: pi,
            description: `yoagent's ${yo}`,
            parameters: { type: 'object' },
            exposure: 'direct',
            sourceInfo: { path: `builtin:${pi}`, source: 'builtin', scope: 'user', origin: 'top-level' },
          })),
        ...[...tools.values()].map((tool) => ({
          name: tool.name,
          description: tool.description,
          parameters: tool.parameters,
          promptGuidelines: tool.promptGuidelines,
          exposure: tool.exposure ?? 'direct',
          sourceInfo: { path: 'extension', source: 'extension', scope: 'temporary', origin: 'top-level' },
        })),
      ]
    }
    // Session writes go to the adapter's in-memory session, so a tool that records or shows its
    // result this way (pi video tools do, after the paid work) does not fail. They are pi state,
    // read back through `ctx.sessionManager`; yoagent's own history is written only by its host.
    runtime.appendEntry = (customType: string, data?: unknown) => {
      sessionManager.appendCustomEntry(customType, data)
    }
    runtime.sendMessage = (
      message: { customType: string; content: string | Block[]; display?: boolean; details?: unknown },
      options?: { triggerTurn?: boolean; deliverAs?: string },
    ) => {
      // As pi's own runtime stores it: a `custom_message` entry.
      sessionManager.appendCustomMessageEntry(message.customType, message.content ?? [], message.display, message.details)
      if (message.display) {
        const content = message.content
        const shown =
          typeof content === 'string' ? content : Array.isArray(content) ? text(content) : JSON.stringify(content)
        report('info', `[pi ${message.customType}] ${shown}`)
      }
      if (options?.triggerTurn || options?.deliverAs) {
        report('warn', `[pi] ${message.customType}: a message cannot start or join a turn in yoagent; recorded only`)
      }
    }
    for (const action of UNSUPPORTED_ACTIONS) {
      runtime[action] = () => {
        throw new Error(`pi.${action}() is not available in yoagent`)
      }
    }
    runtime.setModel = () => Promise.reject(new Error('pi.setModel() is not available in yoagent'))

    // What does not map: reported once each; deciding events refuse.
    const reported = new Set<string>()
    const checkUnmapped = (atLoad: boolean) => {
      const deciding: string[] = []
      const ignored: string[] = []
      for (const ext of extensions) {
        const own = (what: string) => `${short(ext.path)}: ${what}`
        for (const event of ext.handlers.keys()) {
          if (MAPPED_EVENTS.has(event)) continue
          if (watching && CHECKED_EVENTS.has(event)) continue
          if (observed(event)) continue
          if (DECIDING_EVENTS.has(event) && !allowUnmapped.has(event)) deciding.push(own(`event "${event}"`))
          else ignored.push(own(`event "${event}"`))
        }
        ignored.push(
          ...[...ext.commands.keys()].map((c) => own(`command /${c}`)),
          ...[...ext.shortcuts.keys()].map((s) => own(`shortcut ${s}`)),
          ...[...ext.flags.keys()].map((f) => own(`flag --${f}`)),
          ...(ext.messageRenderers.size + (ext.toolRenderers?.length ?? 0) + (ext.entryRenderers?.size ?? 0) > 0
            ? [own('renderers')]
            : []),
          ...(runtime.pendingProviderRegistrations ?? [])
            .filter((p) => p.extensionPath === ext.path)
            .map((p) => own(`model provider ${p.name}`)),
          ...(runtime.pendingNativeProviderRegistrations ?? [])
            .filter((p) => p.extensionPath === ext.path)
            .map((p) => own(`model provider ${p.provider.id}`)),
          ...(runtime.pendingVirtualModelRegistrations ?? [])
            .filter((v) => v.extensionPath === ext.path)
            .map((v) => own(`virtual model ${v.definition.provider}/${v.definition.id}`)),
          ...(runtime.mcpServers?.list() ?? [])
            .filter((s) => s.extensionPath === ext.path)
            .map((s) => own(`MCP server ${s.name}`)),
        )
      }
      const fresh = (list: string[]) => list.filter((what) => !reported.has(what) && reported.add(what))
      const newDeciding = fresh(deciding)
      const newIgnored = fresh(ignored)
      if (newDeciding.length > 0) {
        const text =
          `pi extensions handle events yoagent never fires, which would decide or rewrite what the agent does: ` +
          `${newDeciding.join('; ')} (list an event in allowUnmapped to accept that it goes unenforced)`
        if (atLoad) throw new Error(text)
        refusal = `the pi extensions adapter stopped: ${text}`
        report('warn', `[pi] ${refusal}`)
      }
      if (newIgnored.length > 0) {
        const text = `not available in yoagent, ignored: ${newIgnored.join('; ')}`
        if (atLoad && config.strict) throw new Error(`pi extensions use what does not map — ${text}`)
        // Expected, not a fault: app features (commands, shortcuts, events yoagent never fires)
        // a library has no place for. Refusals, which are faults, stay warnings.
        report('info', `[pi] ${text}`)
      }
    }

    // A pi tool named exactly like a yoagent built-in: if the host also installs that one, it is
    // the one that runs (yoagent's own tools win the merge), unjudged as the pi tool.
    const checkShadowing = (atLoad: boolean) => {
      for (const name of available().keys()) {
        if (!names.has(name) || withoutBuiltins.has(name) || reported.has(`shadow:${name}`)) continue
        reported.add(`shadow:${name}`)
        const text =
          `a pi extension's tool "${name}" is named like yoagent's built-in "${name}"; if the host installs that ` +
          `one too, it runs instead of the pi tool. Leave yoagent's out and list it in withoutBuiltins`
        if (atLoad) throw new Error(text)
        refusal = `the pi extensions adapter stopped: ${text}`
        report('warn', `[pi] ${refusal}`)
      }
    }

    // `message_end` / `context` handlers present at load: their checks need the run's
    // conversation, asked for at registration (it cannot be asked for later).
    let watching = false
    // Per run: the messages (canonical JSON) the `message_end` handlers have seen.
    const ended = new Map<string, Set<string>>()
    // Per run: checks in order (parallel tool calls share one assistant message).
    const queue = new Map<string, Promise<unknown>>()
    const inOrder = <T>(runId: string, task: () => Promise<T>): Promise<T> => {
      const next = (queue.get(runId) ?? Promise.resolve()).then(task, task)
      queue.set(runId, next.catch(() => undefined))
      return next
    }
    /** A check the call's cancellation cut short (the hook timed out or the run stopped): not passed. */
    const unfinished = (event: string) => `pi's ${event} check did not finish (timed out or cancelled), so this does not go ahead`
    const stopWith = (text: string) => {
      refusal = `the pi extensions adapter stopped: ${text}`
      report('warn', `[pi] ${refusal}`)
      return refusal
    }
    /** `message_end` for each message of the run not yet seen; the refusal if one would rewrite it. */
    const checkEnded = (runId: string, messages: Message[] | undefined, signal?: AbortSignal) =>
      inOrder(runId, async () => {
        if (refusal) return refusal
        const listeners = handlers('message_end')
        if (!messages || listeners.length === 0) return
        let seen = ended.get(runId)
        if (!seen) ended.set(runId, (seen = new Set()))
        for (const original of messages) {
          const key = canon(seenAs(original))
          if (seen.has(key)) continue
          for (const { ext, fn } of listeners) {
            const copy = piMessage(original)
            const before = canon(copy)
            let result: { message?: unknown } | undefined
            try {
              result = (await fn({ type: 'message_end', message: copy }, context(signal, runId))) as typeof result
            } catch (error) {
              // Cut short: not judged, so not seen; the next check runs it again.
              if (signal?.aborted) return unfinished('message_end')
              report('warn', `[pi] ${short(ext.path)} message_end: ${message(error)}`)
              continue
            }
            if (signal?.aborted) return unfinished('message_end')
            const replaced = result?.message !== undefined && canon(result.message) !== before
            if (replaced || canon(copy) !== before) {
              return stopWith(`${short(ext.path)} rewrites a ${original.role} message in message_end, which yoagent cannot apply`)
            }
          }
          seen.add(key)
        }
      })
    /** `context` on the messages a request sends; the refusal if a handler would change them. */
    const checkContext = async (runId: string, messages: Message[] | undefined, signal?: AbortSignal) => {
      const listeners = handlers('context')
      if (!messages || listeners.length === 0) return
      for (const { ext, fn } of listeners) {
        const copy = messages.map(piMessage)
        const before = canon(copy)
        let result: { messages?: unknown } | undefined
        try {
          result = (await fn({ type: 'context', messages: copy }, context(signal, runId))) as typeof result
        } catch (error) {
          if (signal?.aborted) return unfinished('context')
          report('warn', `[pi] ${short(ext.path)} context: ${message(error)}`)
          continue
        }
        if (signal?.aborted) return unfinished('context')
        const replaced = result?.messages !== undefined && canon(result.messages) !== before
        if (replaced || canon(copy) !== before) {
          return stopWith(`${short(ext.path)} changes the context in its context handler, which yoagent cannot apply`)
        }
      }
    }

    // Notes from `before_agent_start`, computed once per run.
    const notes = new Map<string, Promise<string | undefined>>()
    // Runs a `{ block, terminate: true }` asked to stop.
    const terminated = new Map<string, string>()
    // The arguments the policies approved, per run and call: `call_tool` runs only these.
    const judged = new Map<string, string>()
    // Per run, as its `tools` saw them: the pi tools it was offered, the ones its pi tools may
    // call through `ctx.executeTool`, and the extension file that registered each (what
    // `nestedCalls` compares). Its calls, top-level and nested, are judged and run against
    // these only, so a name registered again later (an extension earlier in the list, from an
    // event handler or a timer) never takes over a call, and `source` names what runs. Frozen
    // shallow copies: `ctx.tools` hands them to running tools, which must not swap an `execute`.
    const snapshots = new Map<
      string,
      { offered: Map<string, PiTool>; callable: Map<string, PiTool>; sources: Map<string, string> }
    >()
    const frozen = (tools: Map<string, PiTool>) =>
      new Map([...tools].map(([name, tool]) => [name, Object.freeze({ ...tool }) as PiTool]))
    const offeredIn = (runId: string) => snapshots.get(runId)?.offered ?? new Map<string, PiTool>()
    const callableIn = (runId: string) => snapshots.get(runId)?.callable ?? new Map<string, PiTool>()
    /** The extension file that registered `name`, as the run's `tools` saw it. */
    const sourceIn = (runId: string, name: string) => snapshots.get(runId)?.sources.get(name)
    /** Why `nestedCalls` refuses a call from a tool of `caller` to `name`, if it does. */
    const nestedRefusal = (runId: string, caller: string | undefined, name: string) => {
      if (nestedCalls === 'off') return `nested tool calls are turned off by the host (nestedCalls: "off")`
      if (nestedCalls === 'own-extension' && (caller === undefined || sourceIn(runId, name) !== caller)) {
        return `${name} belongs to another extension; the host allows nested calls only within the calling tool's own extension (nestedCalls: "own-extension")`
      }
      return undefined
    }
    const callKey = (call: ToolCall) => `${call.run_id}/${call.call_id}`

    const startNote = async (prompt: string, signal: AbortSignal, runId: string) => {
      let systemPrompt = PROMPT_MARK
      for (const { ext, fn } of handlers('before_agent_start')) {
        const options = new Proxy({} as Record<string, unknown>, {
          get: () => undefined,
          set: () => {
            throw new Error('changing systemPromptOptions is not available in yoagent; add text to systemPrompt instead')
          },
        })
        const event = { type: 'before_agent_start', prompt, systemPrompt, systemPromptOptions: options }
        try {
          const result = (await fn(event, context(signal, runId, systemPrompt))) as
            | { systemPrompt?: unknown; message?: unknown }
            | undefined
          if (result?.message !== undefined) {
            report('warn', `[pi] ${short(ext.path)} before_agent_start: its message dropped (yoagent cannot inject one)`)
          }
          if (result?.systemPrompt !== undefined) {
            if (typeof result.systemPrompt !== 'string' || !result.systemPrompt.includes(PROMPT_MARK)) {
              throw new Error('replaced the system prompt; yoagent only takes text added to it')
            }
            systemPrompt = result.systemPrompt
          }
        } catch (error) {
          // As in pi: one handler's failure is reported, the others still count.
          report('warn', `[pi] ${short(ext.path)} before_agent_start skipped, so it does not apply: ${message(error)}`)
        }
      }
      const added = systemPrompt.split(PROMPT_MARK).map((part) => part.trim()).filter(Boolean)
      return added.length > 0 ? added.join('\n\n') : undefined
    }

    // Before registering: tools an extension adds at session start are offered from the first run.
    const started: string[] = []
    for (const { ext, fn } of handlers('session_start')) {
      try {
        await fn({ type: 'session_start', reason: 'startup' }, context())
      } catch (error) {
        started.push(`${short(ext.path)}: ${message(error)}`)
      }
    }
    // Set once the observers exist: stops them and waits (bounded) for what is queued, so a
    // tracer's `agent_end` comes before its `session_shutdown`, as in pi.
    let drainObservers: () => Promise<void> = async () => {}
    // Registered first, so a refusal below still shuts the extensions down.
    ctx.effect(async () => {
      await drainObservers()
      for (const { ext, fn } of handlers('session_shutdown')) {
        try {
          await fn({ type: 'session_shutdown', reason: 'quit' }, context())
        } catch (error) {
          report('warn', `[pi] ${short(ext.path)} session_shutdown: ${message(error)}`)
        }
      }
    })
    if (started.length > 0) {
      // An extension whose setup failed may be missing its policies.
      throw new Error(`pi extensions failed in session_start: ${started.join('; ')}`)
    }
    // After session_start, so what it registered is checked too.
    // Observer events: the yoagent event types the handlers present at load need. One registered
    // later is fired when its types are already subscribed, else reported as not available.
    // (After session_start, like `watching`: what it registered counts.)
    const subscribed = new Set<string>()
    for (const [event, types] of OBSERVED_EVENTS) {
      if (handlers(event).length > 0) for (const t of types) subscribed.add(t)
    }
    const observed = (event: string) =>
      OBSERVED_EVENTS.has(event) && OBSERVED_EVENTS.get(event)!.every((t) => subscribed.has(t))
    watching = handlers('context').length + handlers('message_end').length > 0
    checkUnmapped(true)
    checkShadowing(true)

    /** Per run: the turn index, and each running tool call's pi name and arguments. */
    const turns = new Map<string, number>()
    /** A run's tools kept past `finish` for its `agentEnd` (delivered after it), when subscribed. */
    const ending = new Map<string, { offered: Map<string, PiTool> }>()
    const running = new Map<string, { toolName: string; args: unknown }>()
    const observerFailed = new Set<string>()
    /** A yoagent tool call as pi names it: a pi tool keeps its name, a built-in gets pi's name and argument shape. */
    const piCall = (runId: string, tool: string, args: unknown) => {
      const own = (snapshots.get(runId) ?? ending.get(runId))?.offered.has(tool) ?? false
      const shape = own ? undefined : ARGS.get(tool)
      let input = args
      try {
        input = shape && args && typeof args === 'object' ? shape.toPi(args as Args) : args
      } catch (error) {
        // An argument shape that cannot take pi's form: shown in yoagent's.
        report('debug', `[pi] ${tool}: arguments shown in yoagent's form to observers: ${message(error)}`)
      }
      return { toolName: own ? tool : (names.get(tool) ?? tool), args: input }
    }
    /** A message as pi's observers see it: its tool calls and results under pi's names and shapes. */
    const observedMessage = (runId: string, message: Message) => {
      const out = piMessage(message) as Record<string, unknown> & { role: string; content?: unknown }
      if (out.role === 'assistant' && Array.isArray(out.content)) {
        out.content = out.content.map((block: Record<string, unknown>) => {
          if (block?.type !== 'toolCall' || typeof block.name !== 'string') return block
          const call = piCall(runId, block.name, block.arguments)
          return { ...block, name: call.toolName, arguments: call.args }
        })
      }
      if (out.role === 'toolResult' && typeof out.toolName === 'string') {
        out.toolName = piCall(runId, out.toolName, undefined).toolName
      }
      return out
    }
    const llm = (runId: string, messages: unknown) =>
      ((messages as Message[] | undefined) ?? [])
        .filter((m) => m && m.role !== ('extension' as never))
        .map((m) => observedMessage(runId, m))
    /** The pi event for a yoagent one, or nothing (the types a subscribed observer does not use). */
    const piEvent = (event: Record<string, unknown> & { type: string; run: RunInfo }): Record<string, unknown> | undefined => {
      const runId = event.run.run_id
      switch (event.type) {
        case 'agentStart':
          turns.set(runId, 0)
          return { type: 'agent_start' }
        case 'agentEnd': {
          turns.delete(runId)
          for (const key of running.keys()) if (key.startsWith(`${runId}/`)) running.delete(key)
          const messages = llm(runId, event.messages)
          ending.delete(runId)
          return { type: 'agent_end', messages }
        }
        case 'turnStart':
          return { type: 'turn_start', turnIndex: turns.get(runId) ?? 0, timestamp: Date.now() }
        case 'turnEnd': {
          const turnIndex = turns.get(runId) ?? 0
          turns.set(runId, turnIndex + 1)
          const message = event.message as Message
          // As pi: only a turn that ended with an assistant message (not a stop or cancel marker).
          if (message?.role !== 'assistant') return undefined
          const stop = (message as { stopReason?: string }).stopReason
          const toolResults = llm(runId, event.toolResults)
          return {
            type: 'turn_end',
            turnIndex,
            message: observedMessage(runId, message),
            toolResults,
            // No session entries in yoagent: ids for this run and turn, and an empty boundary.
            messageEntryId: `${runId}/turn-${turnIndex}`,
            toolResultEntryIds: toolResults.map((_, i) => `${runId}/turn-${turnIndex}/result-${i}`),
            entries: [],
            continue: false,
            context: { contextEntries: [], contextMessages: [], llmMessages: [], pendingMessages: [], canContinue: false },
            outcome: stop === 'aborted' ? 'aborted' : stop === 'error' ? 'error' : 'completed',
          }
        }
        case 'messageStart':
          return { type: 'message_start', message: observedMessage(runId, event.message as Message) }
        case 'messageUpdate': {
          const message = observedMessage(runId, event.message as Message) as { content?: unknown[] }
          const delta = event.delta as { type: string; delta: string }
          return {
            type: 'message_update',
            message,
            assistantMessageEvent: {
              type: DELTA_TYPES[delta.type] ?? `${delta.type}_delta`,
              contentIndex: Math.max(0, (message.content?.length ?? 1) - 1),
              delta: delta.delta,
              partial: message,
            },
          }
        }
        case 'toolExecutionStart': {
          const call = piCall(runId, event.toolName as string, event.args)
          running.set(`${runId}/${event.toolCallId}`, call)
          return { type: 'tool_execution_start', toolCallId: event.toolCallId, ...call }
        }
        case 'toolExecutionUpdate': {
          const call = running.get(`${runId}/${event.toolCallId}`) ?? piCall(runId, event.toolName as string, undefined)
          return { type: 'tool_execution_update', toolCallId: event.toolCallId, ...call, partialResult: event.partialResult }
        }
        case 'toolExecutionEnd': {
          const key = `${runId}/${event.toolCallId}`
          const call = running.get(key) ?? piCall(runId, event.toolName as string, undefined)
          running.delete(key)
          return {
            type: 'tool_execution_end',
            toolCallId: event.toolCallId,
            toolName: call.toolName,
            result: event.result,
            isError: event.isError,
          }
        }
        default:
          return undefined
      }
    }
    /**
     * Fires a yoagent event's pi counterpart at its handlers. The pi event is built now (the
     * run's tools are still known), the handlers run later on one ordered chain: `on_event`
     * returns at once, so a slow observer (a tracer flushing over the network) never meets the
     * bridge's per-event timeout, which would switch every observer off for the run (and fail a
     * required bridge's run). At most MAX_OBSERVER_BACKLOG events wait; past that, events are
     * dropped, reported once. Never throws: a throwing handler is reported once (per extension
     * and event) and the others still run; a result is ignored (a `turn_end` asking to continue
     * or add entries is reported once).
     */
    let chain: Promise<void> = Promise.resolve()
    let backlog = 0
    let dropped = 0
    let closed = false
    const mappingFailed = new Set<string>()
    /** Once per run, extension and kind of trouble: a failure in a later run is reported again. */
    const once = (key: string, level: 'warn' | 'info', text: string) => {
      if (observerFailed.has(key)) return
      observerFailed.add(key)
      report(level, text)
    }
    const runHandlers = async (pi: Record<string, unknown>, runId: string) => {
      for (const { ext, fn } of handlers(pi.type as string)) {
        let timer: ReturnType<typeof setTimeout> | undefined
        try {
          const call = Promise.resolve().then(() => fn(structuredClone(pi), context(undefined, runId)))
          const budget = new Promise<'late'>((resolve) => {
            timer = setTimeout(() => resolve('late'), OBSERVER_HANDLER_BUDGET_MS)
          })
          const result = (await Promise.race([call, budget])) as
            | 'late'
            | { continue?: unknown; entries?: unknown[] }
            | undefined
          if (result === 'late') {
            // Left running; the queue moves on so one stuck handler cannot stall every observer.
            call.catch(() => {})
            once(
              `${runId}\0${ext.path}\0${pi.type}\0late`,
              'warn',
              `[pi] ${short(ext.path)}: a ${pi.type} handler did not finish within ${OBSERVER_HANDLER_BUDGET_MS / 1000} s; ` +
                `the observers went on without it (reported once per run)`,
            )
          } else if (pi.type === 'turn_end' && result && (result.continue || (result.entries?.length ?? 0) > 0)) {
            once(
              `${ext.path}\0turn_end-result`,
              'warn',
              `[pi] ${short(ext.path)}: a turn_end result (continue / entries) is not applied in yoagent; ignored`,
            )
          }
        } catch (error) {
          once(
            `${runId}\0${ext.path}\0${pi.type}`,
            'warn',
            `[pi] ${short(ext.path)}: a ${pi.type} handler threw (reported once per run): ${message(error)}`,
          )
        } finally {
          clearTimeout(timer)
        }
      }
      // The run is over for its observers: its once-per-run keys can go.
      if (pi.type === 'agent_end') {
        for (const key of observerFailed) if (key.startsWith(`${runId}\0`)) observerFailed.delete(key)
      }
    }
    /**
     * Fires a yoagent event's pi counterpart at its handlers. The pi event is built now (the
     * run's tools are still known), the handlers run later on one ordered chain: `on_event`
     * returns at once, so a slow observer (a tracer flushing over the network) never meets the
     * bridge's per-event timeout, which would switch every observer off for the run (and fail a
     * required bridge's run). A handler gets OBSERVER_HANDLER_BUDGET_MS before the queue moves
     * on. Past MAX_OBSERVER_BACKLOG waiting events streaming events are dropped (lifecycle ones
     * past twice that), counted and reported when the queue catches up. Never throws: a
     * throwing handler is reported once per run and the others still run; a result is ignored
     * (a `turn_end` asking to continue or add entries is reported once).
     */
    const observe = async (event: Record<string, unknown> & { type: string; run: RunInfo }) => {
      if (refusal || closed) {
        ending.clear()
        return
      }
      let pi: Record<string, unknown> | undefined
      try {
        pi = piEvent(event)
      } catch (error) {
        if (!mappingFailed.has(event.type)) {
          mappingFailed.add(event.type)
          report('warn', `[pi] could not map ${event.type} events for pi observers (reported once): ${message(error)}`)
        }
        return
      }
      if (!pi) return
      const limit = STREAMING_EVENTS.has(pi.type as string) ? MAX_OBSERVER_BACKLOG : 2 * MAX_OBSERVER_BACKLOG
      if (backlog >= limit) {
        if (dropped++ === 0) {
          report('warn', `[pi] observers are ${backlog} events behind; dropping events until they catch up`)
        }
        return
      }
      backlog++
      const next = pi
      const runId = event.run.run_id
      chain = chain
        .then(() => runHandlers(next, runId))
        .catch((error) => report('warn', `[pi] observer queue: ${message(error)}`))
        .finally(() => {
          backlog--
          if (backlog === 0 && dropped > 0) {
            report('warn', `[pi] observers caught up; ${dropped} events were dropped`)
            dropped = 0
          }
        })
    }
    drainObservers = async () => {
      closed = true
      let timer: ReturnType<typeof setTimeout> | undefined
      const done = await Promise.race([
        chain.then(() => true),
        new Promise<false>((resolve) => {
          timer = setTimeout(() => resolve(false), OBSERVER_DRAIN_MS)
        }),
      ])
      clearTimeout(timer)
      if (!done) report('warn', `[pi] unloading with ${backlog} observer events still queued`)
    }

    ctx.effect(
      yoagent.register(config.name ?? 'pi-extensions', {
        async tools(run: RunInfo): Promise<ToolSpec[]> {
          // Registrations made since load: reported now (a run cannot refuse the load).
          checkUnmapped(false)
          checkShadowing(false)
          if (refusal) return []
          const paths = registeredBy()
          const tools = frozen(available())
          snapshots.set(run.run_id, { offered: tools, callable: frozen(callable()), sources: paths })
          return [...tools.values()].map((tool) => ({
            name: tool.name,
            // The extension file that registered it: a host's policy may trust a tool by it.
            source: paths.get(tool.name) ?? null,
            label: tool.label ?? null,
            description: [tool.description ?? '', ...(tool.promptGuidelines ?? []).map((g) => `- ${g}`)]
              .filter(Boolean)
              .join('\n'),
            parameters: tool.parameters,
          }))
        },

        async call_tool(call: ToolCall & Cancellable): Promise<ToolResult> {
          // The definition this run was offered, whatever registered the name since.
          const tool = offeredIn(call.run_id).get(call.tool)
          if (!tool) return { text: `pi tool ${call.tool} is not available`, is_error: true }
          const approved = judged.get(callKey(call))
          judged.delete(callKey(call))
          if (approved === undefined || canon(call.args) !== approved) {
            return {
              text: `not run: the arguments of ${call.tool} changed after pi's policies judged them`,
              is_error: true,
            }
          }
          try {
            const out = await tool.execute(call.call_id, call.args, call.signal, undefined, toolContext(call.signal, call.run_id, call.call_id, 0, sourceIn(call.run_id, call.tool)))
            if (out.isError === true) return { text: text(out.content), details: out.details ?? null, is_error: true }
            return { content: blocks(out.content), details: out.details ?? null }
          } catch (error) {
            return { text: message(error), is_error: true }
          }
        },

        async before_tool(call: ToolCall & Cancellable) {
          // Registrations made during the run: caught before the next call, not the next run.
          checkUnmapped(false)
          checkShadowing(false)
          if (refusal) return { deny: refusal }
          // The message making this call ended: its message_end handlers judge it before any tool runs.
          if (watching) {
            const stopped = await checkEnded(call.run_id, call.messages, call.signal)
            if (stopped) return { deny: stopped }
          }
          const piTools = offeredIn(call.run_id)
          const own = piTools.get(call.tool)
          const counterpart = names.get(call.tool)
          // A yoagent built-in whose pi counterpart an extension overrides: the model must use the override.
          if (!own && counterpart && counterpart !== call.tool && piTools.has(counterpart)) {
            return { deny: `a pi extension replaces this tool with "${counterpart}"; call "${counterpart}" instead` }
          }
          const toolName = own ? call.tool : (counterpart ?? call.tool)
          if (inactive(toolName)) {
            return { deny: `"${toolName}" is not an active tool (a pi extension narrowed them with setActiveTools)` }
          }
          const policies = handlers('tool_call')
          const shape = own ? undefined : ARGS.get(call.tool)
          let input: Args
          let base: Args = call.args
          if (own) {
            // As pi's agent loop: prepare, validate, then the policies judge the validated arguments.
            try {
              // On a copy: pi's own prepareArguments (edit's) mutates its argument in place.
              const copy = structuredClone(call.args)
              const prepared = own.prepareArguments ? (own.prepareArguments(copy) as Args) : copy
              input = validateToolArguments(own as never, { name: own.name, arguments: prepared } as never) as Args
            } catch (error) {
              return { deny: message(error) }
            }
          } else {
            // A built-in's relative path is resolved where the extensions look (`ctx.cwd`), so the
            // tool acts there. Only yoagent's own tools: an MCP tool's `path` may be a repository's.
            const builtin = YOAGENT_KEYS.has(call.tool)
            if (builtin && policies.length > 0 && typeof base.path === 'string' && !isAbsolute(base.path)) {
              base = { ...base, path: resolve(cwd, base.path) }
            }
            input = structuredClone(shape ? shape.toPi(base) : base)
          }
          const before = canon(input)
          for (const { ext, fn } of policies) {
            const event = { type: 'tool_call', toolCallId: call.call_id, toolName, input }
            let result: { block?: boolean; reason?: string; terminate?: boolean } | undefined
            try {
              result = (await fn(event, context(call.signal, call.run_id))) as typeof result
            } catch (error) {
              // As in pi: a failing tool_call handler blocks the call.
              return { deny: `pi extension ${short(ext.path)} failed: ${message(error)}` }
            }
            if (result?.block) {
              const reason = result.reason ?? `blocked by pi extension ${short(ext.path)}`
              if (result.terminate === true) terminated.set(call.run_id, reason)
              return { deny: reason }
            }
          }
          if (own) {
            judged.set(callKey(call), canon(input))
            // Prepared or coerced arguments count as a rewrite too.
            return canon(input) === canon(call.args) ? undefined : { args: input }
          }
          if (canon(input) === before && base === call.args) return
          let out: Args
          try {
            out = shape ? shape.fromPi(input) : input
          } catch (error) {
            return { deny: message(error) }
          }
          const keys = YOAGENT_KEYS.get(call.tool)
          if (keys) {
            for (const [key, value] of Object.entries(out)) {
              if (!keys.has(key) && canon(value) !== canon(base[key])) {
                return { deny: `a pi extension set "${key}", which yoagent's ${call.tool} does not have` }
              }
            }
          }
          return { args: out }
        },

        async after_tool(call: ToolCall & Cancellable, output: ToolOutput) {
          if (refusal) throw new Error(refusal)
          const editors = handlers('tool_result')
          if (editors.length === 0) return
          const own = offeredIn(call.run_id).has(call.tool)
          const shape = own ? undefined : ARGS.get(call.tool)
          const event = {
            type: 'tool_result',
            toolCallId: call.call_id,
            toolName: own ? call.tool : (names.get(call.tool) ?? call.tool),
            input: shape ? shape.toPi(call.args) : call.args,
            // yoagent's text and image blocks have pi's shape.
            content: output.content as Block[],
            details: output.details,
            isError: output.is_error,
          }
          const changed = { content: false, details: false, isError: false }
          const failed: string[] = []
          for (const { ext, fn } of editors) {
            try {
              const result = (await fn(event, context(call.signal, call.run_id))) as
                | { content?: Block[]; details?: unknown; isError?: boolean }
                | undefined
              if (result?.content !== undefined) {
                event.content = result.content
                changed.content = true
              }
              if (result?.details !== undefined) {
                event.details = result.details
                changed.details = true
              }
              if (result?.isError !== undefined) {
                event.isError = result.isError
                changed.isError = true
              }
            } catch (error) {
              failed.push(`${short(ext.path)}: ${message(error)}`)
            }
          }
          if (failed.length > 0) {
            // Unlike pi (which skips it): a redaction that failed must not let the raw output through.
            throw new Error(`pi tool_result handlers failed, the result is withheld: ${failed.join('; ')}`)
          }
          if (!changed.content && !changed.details && !changed.isError) return
          return {
            // Only a replaced content is sent back, as blocks (images kept): yoagent's edit replaces every block.
            ...(changed.content ? { content: blocks(event.content) } : {}),
            ...(changed.details ? { details: event.details ?? null } : {}),
            ...(changed.isError ? { is_error: event.isError } : {}),
          }
        },

        async on_input(input) {
          if (refusal) return { reject: refusal }
          for (const { ext, fn } of handlers('input')) {
            const event = { type: 'input', text: input.text, source: 'interactive' }
            let result: { action?: string } | undefined
            try {
              result = (await fn(event, context(input.signal))) as typeof result
            } catch (error) {
              // Unlike pi (which goes on): an input check that failed rejects.
              return { reject: `pi extension ${short(ext.path)} failed: ${message(error)}` }
            }
            if (result?.action === 'handled') return { reject: `handled by pi extension ${short(ext.path)}` }
            if (result?.action === 'transform') {
              return { reject: `pi extension ${short(ext.path)} would rewrite the prompt, which yoagent cannot do` }
            }
          }
        },

        async before_model(turn) {
          if (typeof turn.model === 'string' && turn.model) {
            runModels.set(turn.run_id, turn.model)
            latestModel = turn.model
          }
          checkUnmapped(false)
          checkShadowing(false)
          if (refusal) return { stop: refusal }
          const stop = terminated.get(turn.run_id)
          if (stop) return { stop }
          if (watching) {
            // Prompts and tool results ended since; then the context this request sends.
            const stopped =
              (await checkEnded(turn.run_id, turn.messages, turn.signal)) ?? (await checkContext(turn.run_id, turn.messages, turn.signal))
            if (stopped) return { stop: stopped }
          }
          if (handlers('before_agent_start').length === 0) return
          let note = notes.get(turn.run_id)
          if (!note) {
            note = startNote(turn.latest_user_text ?? '', turn.signal, turn.run_id)
            notes.set(turn.run_id, note)
          }
          const added = await note
          return added ? { note: added } : undefined
        },

        // Only with message_end / context handlers: the answer's message_end check.
        ...(watching
          ? {
              async on_stop(stop: Stop & Cancellable) {
                const stopped = await checkEnded(stop.run_id, stop.messages, stop.signal)
                // The answer was already given and stands. `{fail}` fails the run only when the host
                // made the bridge required (otherwise yoagent logs it); later runs are refused either way.
                if (stopped) return { fail: stopped }
              },
            }
          : {}),

        async finish(outcome) {
          runModels.delete(outcome.run_id)
          notes.delete(outcome.run_id)
          terminated.delete(outcome.run_id)
          ended.delete(outcome.run_id)
          queue.delete(outcome.run_id)
          for (const key of judged.keys()) if (key.startsWith(`${outcome.run_id}/`)) judged.delete(key)
          // `agentEnd` comes after `finish`: it names tools from what this run offered.
          const snapshot = snapshots.get(outcome.run_id)
          if (snapshot && subscribed.has('agentEnd')) ending.set(outcome.run_id, snapshot)
          snapshots.delete(outcome.run_id)
          // Observer bookkeeping: whatever event types are subscribed (the bridge delivers the
          // run's events before `finish`; only `agentEnd`, which needs neither, comes after).
          turns.delete(outcome.run_id)
          for (const key of running.keys()) if (key.startsWith(`${outcome.run_id}/`)) running.delete(key)
        },
        // Only with observer handlers at load: pi's observer events, from yoagent's.
        ...(subscribed.size > 0 ? { on_event: observe } : {}),
      }, {
        ...(watching ? { messages: true } : {}),
        ...(subscribed.size > 0 ? { events: [...subscribed] } : {}),
      }),
    )
  },
})
