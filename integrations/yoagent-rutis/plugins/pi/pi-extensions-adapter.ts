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
//                           run. A throw or `isError` is an error result
//                           (non-text blocks become `[image block]` text).
//                           A tool that overrides one of pi's built-ins
//                           (`read`, `edit`, ...) makes the adapter deny
//                           yoagent's counterpart (`read_file`, ...), so the
//                           model cannot go around it. A tool named exactly
//                           like a yoagent built-in (pi's sandboxed `bash`)
//                           refuses the load unless the host says it left
//                           that built-in out (`withoutBuiltins`).
//   pi.setActiveTools       an allowlist: tools outside it are not offered
//                           and their calls (yoagent's built-ins under pi's
//                           names) are denied. `getActiveTools` /
//                           `getAllTools` answer from the same view.
//   on("tool_call")         `before_tool`, for every call of the run. yoagent's
//                           built-ins are judged under pi's names (TOOL_NAMES),
//                           their arguments translated both ways and a
//                           relative `path` resolved against `cwd` first, so
//                           the tool acts where the policy looked. `{ block }`
//                           denies (`terminate: true` also stops the run at
//                           its next model request); in-place changes to
//                           `event.input` rewrite the arguments (a field the
//                           yoagent tool does not have denies the call); a
//                           handler that throws blocks, as in pi.
//   on("tool_result")       `after_tool`: changes to content, details and
//                           isError are chained; only the fields a handler
//                           set are applied (a replaced content keeps only its
//                           text). A handler that throws withholds the result
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
//
// What does not map: no other event is ever fired. Events that would decide
// or rewrite something yoagent does (DECIDING_EVENTS: `context`,
// `message_end`, `before_provider_request`, ...) refuse the load unless
// listed in `allowUnmapped`; the rest (observers such as `agent_end` or
// `tool_execution_*`, pi's session events, the boundary events `turn_end` /
// `agent_before_settle`), commands, shortcuts, flags, renderers, model
// providers, virtual models and MCP servers are reported (a warning, or a
// load failure with `strict`). One registered after load is reported then,
// and a deciding one stops the adapter: every later call denied. Other
// runtime actions (`pi.sendMessage`, `pi.appendEntry`, ...) throw "not
// available in yoagent". `ctx.hasUI` is false and `ctx.ui` behaves as in
// pi's print mode: `confirm` answers false, `select` and `input` nothing, so
// a policy that would ask the user denies instead.

import { isAbsolute, resolve } from 'node:path'
import { definePlugin } from '@arcships/rutis'
import type { Cancellable, ToolCall, ToolOutput, ToolResult, ToolSpec, Yoagent } from '../yoagent.d.ts'

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

/** Unfired events whose handlers would decide or rewrite something yoagent does: they refuse the load. */
const DECIDING_EVENTS = new Set([
  'context',
  'context_with_system',
  'message_end',
  'before_provider_request',
  'before_provider_headers',
])

/** pi runtime actions with no yoagent counterpart. */
const UNSUPPORTED_ACTIONS = [
  'sendMessage',
  'sendUserMessage',
  'appendEntry',
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

/** pi's print-mode UI (its `noOpUIContext`): nothing to show, no one to ask. */
const NO_UI = new Proxy(
  {
    select: async () => undefined,
    confirm: async () => false,
    input: async () => undefined,
    editor: async () => undefined,
    custom: async () => undefined,
    notify: (message: string, level?: string) => console.warn(`[pi ${level ?? 'info'}] ${message}`),
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

interface Block {
  type: string
  text?: string
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

/**
 * pi's loader, by file: the package exports only `discoverAndLoadExtensions`,
 * which also loads `<cwd>/.pi/extensions` and `~/.pi/agent/extensions`.
 */
async function piLoader(): Promise<{
  loadExtensions(paths: string[], cwd: string): Promise<{
    extensions: PiExtension[]
    errors: { path: string; error: string }[]
    warnings?: { path: string; warning: string }[]
    runtime: PiRuntime
  }>
}> {
  const index = import.meta.resolve('@earendil-works/pi-coding-agent')
  return import(new URL('./core/extensions/loader.js', index).href)
}

const short = (path: string) => path.split('/').pop() ?? path

function text(content: Block[] | undefined): string {
  return (content ?? []).map((block) => (block.type === 'text' ? (block.text ?? '') : `[${block.type} block]`)).join('\n')
}

/** JSON with object keys sorted: arguments cross serde_json, which may reorder them. */
function canon(value: unknown): string {
  return JSON.stringify(value, (_key, v) =>
    v && typeof v === 'object' && !Array.isArray(v)
      ? Object.fromEntries(Object.entries(v as Args).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)))
      : v,
  )
}

const message = (error: unknown) => (error instanceof Error ? error.message : String(error))

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
    },
  },
  async apply(ctx, config) {
    const yoagent = ctx.use<Yoagent>('yoagent')
    const cwd = config.cwd ?? process.cwd()
    const names = new Map(Object.entries({ ...TOOL_NAMES, ...config.toolNames }))
    const withoutBuiltins = new Set(config.withoutBuiltins ?? [])
    const allowUnmapped = new Set(config.allowUnmapped ?? [])

    const { SessionManager } = await import('@earendil-works/pi-coding-agent')
    const { validateToolArguments } = await import('@earendil-works/pi-ai')
    const loaded = await (await piLoader()).loadExtensions(config.extensions, cwd)
    if (loaded.errors.length > 0) {
      throw new Error(`pi extensions failed to load: ${loaded.errors.map((e) => `${e.path}: ${e.error}`).join('; ')}`)
    }
    for (const w of loaded.warnings ?? []) console.warn(`[pi] ${short(w.path)}: ${w.warning}`)
    const extensions = loaded.extensions
    const runtime = loaded.runtime

    /**
     * Set when something that would have to be enforced turns up after load:
     * from then on every tool call is denied, every prompt rejected, every
     * run stopped.
     */
    let refusal: string | undefined

    const sessionManager = SessionManager.inMemory(cwd)
    const context = (signal?: AbortSignal, systemPrompt = '') => ({
      ui: NO_UI,
      mode: 'print',
      hasUI: false,
      cwd,
      sessionManager,
      modelRegistry: undefined,
      model: undefined,
      scopedModels: [],
      signal,
      isIdle: () => signal === undefined,
      // pi's own default is true; this adapter loads only the files it is given, never a project's `.pi`.
      isProjectTrusted: () => false,
      hasPendingMessages: () => false,
      getContextUsage: () => undefined,
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
    })
    const toolContext = (signal: AbortSignal) => ({
      ...context(signal),
      tools: [],
      executeTool: async () => {
        throw new Error('ctx.executeTool() is not available in yoagent')
      },
    })

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

    // pi.setActiveTools: an allowlist of pi names (yoagent's built-ins under TOOL_NAMES).
    let active: Set<string> | undefined
    /** The tools pi would offer: activated on registration, and in the allowlist if one was set. */
    const available = () =>
      new Map(
        [...registered()].filter(([name, tool]) => {
          const exposure = tool.exposure ?? 'direct'
          const activated = (exposure === 'direct' || exposure === 'model-only') && tool.defaultActive !== false
          return activated && (!active || active.has(name))
        }),
      )

    // The runtime actions: tool activation is pi's to decide here; the rest has no counterpart.
    runtime.setActiveTools = (toolNames: string[]) => {
      active = new Set(toolNames)
    }
    runtime.getActiveTools = () =>
      active ? [...active] : [...new Set([...names.values(), ...available().keys()])]
    runtime.getAllTools = () => [
      ...[...names].map(([yo, pi]) => ({
        name: pi,
        description: `yoagent's ${yo}`,
        parameters: { type: 'object' },
        exposure: 'direct',
        sourceInfo: { path: 'yoagent' },
      })),
      ...[...registered().values()].map((tool) => ({
        name: tool.name,
        description: tool.description,
        parameters: tool.parameters,
        promptGuidelines: tool.promptGuidelines,
        exposure: tool.exposure ?? 'direct',
        sourceInfo: { path: 'pi extension' },
      })),
    ]
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
        console.warn(`[pi] ${refusal}`)
      }
      if (newIgnored.length > 0) {
        const text = `not available in yoagent, ignored: ${newIgnored.join('; ')}`
        if (atLoad && config.strict) throw new Error(`pi extensions use what does not map — ${text}`)
        console.warn(`[pi] ${text}`)
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
        console.warn(`[pi] ${refusal}`)
      }
    }

    // Notes from `before_agent_start`, computed once per run.
    const notes = new Map<string, Promise<string | undefined>>()
    // Runs a `{ block, terminate: true }` asked to stop.
    const terminated = new Map<string, string>()
    // The arguments the policies approved, per run and call: `call_tool` runs only these.
    const judged = new Map<string, string>()
    const callKey = (call: ToolCall) => `${call.run_id}/${call.call_id}`

    const startNote = async (prompt: string, signal: AbortSignal) => {
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
          const result = (await fn(event, context(signal, systemPrompt))) as
            | { systemPrompt?: unknown; message?: unknown }
            | undefined
          if (result?.message !== undefined) {
            console.warn(`[pi] ${short(ext.path)} before_agent_start: its message dropped (yoagent cannot inject one)`)
          }
          if (result?.systemPrompt !== undefined) {
            if (typeof result.systemPrompt !== 'string' || !result.systemPrompt.includes(PROMPT_MARK)) {
              throw new Error('replaced the system prompt; yoagent only takes text added to it')
            }
            systemPrompt = result.systemPrompt
          }
        } catch (error) {
          // As in pi: one handler's failure is reported, the others still count.
          console.warn(`[pi] ${short(ext.path)} before_agent_start skipped, so it does not apply: ${message(error)}`)
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
    // Registered first, so a refusal below still shuts the extensions down.
    ctx.effect(async () => {
      for (const { ext, fn } of handlers('session_shutdown')) {
        try {
          await fn({ type: 'session_shutdown', reason: 'quit' }, context())
        } catch (error) {
          console.warn(`[pi] ${short(ext.path)} session_shutdown: ${message(error)}`)
        }
      }
    })
    if (started.length > 0) {
      // An extension whose setup failed may be missing its policies.
      throw new Error(`pi extensions failed in session_start: ${started.join('; ')}`)
    }
    // After session_start, so what it registered is checked too.
    checkUnmapped(true)
    checkShadowing(true)

    ctx.effect(
      yoagent.register(config.name ?? 'pi-extensions', {
        async tools(): Promise<ToolSpec[]> {
          // Registrations made since load: reported now (a run cannot refuse the load).
          checkUnmapped(false)
          checkShadowing(false)
          return [...available().values()].map((tool) => ({
            name: tool.name,
            label: tool.label ?? null,
            description: [tool.description ?? '', ...(tool.promptGuidelines ?? []).map((g) => `- ${g}`)]
              .filter(Boolean)
              .join('\n'),
            parameters: tool.parameters,
          }))
        },

        async call_tool(call: ToolCall & Cancellable): Promise<ToolResult> {
          const tool = available().get(call.tool)
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
            const out = await tool.execute(call.call_id, call.args, call.signal, undefined, toolContext(call.signal))
            return { text: text(out.content), details: out.details ?? null, is_error: out.isError === true }
          } catch (error) {
            return { text: message(error), is_error: true }
          }
        },

        async before_tool(call: ToolCall & Cancellable) {
          if (refusal) return { deny: refusal }
          const piTools = available()
          const own = piTools.get(call.tool)
          const counterpart = names.get(call.tool)
          // A yoagent built-in whose pi counterpart an extension overrides: the model must use the override.
          if (!own && counterpart && counterpart !== call.tool && piTools.has(counterpart)) {
            return { deny: `a pi extension replaces this tool with "${counterpart}"; call "${counterpart}" instead` }
          }
          const toolName = own ? call.tool : (counterpart ?? call.tool)
          if (active && !active.has(toolName)) {
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
            // A relative path is resolved where the extensions look (`ctx.cwd`), so the tool acts there.
            if (policies.length > 0 && typeof base.path === 'string' && !isAbsolute(base.path)) {
              base = { ...base, path: resolve(cwd, base.path) }
            }
            input = structuredClone(shape ? shape.toPi(base) : base)
          }
          const before = canon(input)
          for (const { ext, fn } of policies) {
            const event = { type: 'tool_call', toolCallId: call.call_id, toolName, input }
            let result: { block?: boolean; reason?: string; terminate?: boolean } | undefined
            try {
              result = (await fn(event, context(call.signal))) as typeof result
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
          const own = available().has(call.tool)
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
              const result = (await fn(event, context(call.signal))) as
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
            // Only a replaced content is sent back, as text: yoagent's edit replaces every block.
            ...(changed.content ? { text: text(event.content) } : {}),
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
          if (refusal) return { stop: refusal }
          const stop = terminated.get(turn.run_id)
          if (stop) return { stop }
          if (handlers('before_agent_start').length === 0) return
          let note = notes.get(turn.run_id)
          if (!note) {
            note = startNote(turn.latest_user_text ?? '', turn.signal)
            notes.set(turn.run_id, note)
          }
          const added = await note
          return added ? { note: added } : undefined
        },

        async finish(outcome) {
          notes.delete(outcome.run_id)
          terminated.delete(outcome.run_id)
          for (const key of judged.keys()) if (key.startsWith(`${outcome.run_id}/`)) judged.delete(key)
        },
      }),
    )
  },
})
