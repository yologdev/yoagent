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
// catalog, and list the extension files in its config:
//
//   { "name": "<this file>", "config": { "extensions": ["./my-ext.ts"], "cwd": "/repo" } }
//
// What maps, per pi API (following pi 1.1.0's own runner and agent loop):
//   pi.registerTool         a yoagent tool, offered while pi would activate it
//                           (exposure `direct` / `model-only`, `defaultActive`
//                           not false; the first registration of a name wins,
//                           as in pi). Arguments go through the tool's
//                           `prepareArguments` and pi's validation before any
//                           policy sees them; `execute` gets the bridge's
//                           cancel handle as its signal; a throw or `isError`
//                           is an error result. Tools registered later (in
//                           `session_start`, say) are offered from the next
//                           run on. A tool that overrides one of pi's
//                           built-ins (`read`, `edit`, ...) makes the adapter
//                           deny yoagent's counterpart (`read_file`, ...), so
//                           the model cannot go around it; one named exactly
//                           like a yoagent tool (`bash`) loses to that tool
//                           when the host installs it — the host must not.
//   on("tool_call")         `before_tool`, for every call of the run (the
//                           agent's own built-ins too, under pi's names: see
//                           TOOL_NAMES, arguments translated both ways):
//                           `{ block }` denies with its reason; changes to
//                           `event.input` rewrite the arguments. A handler
//                           that throws blocks, as in pi.
//   on("tool_result")       `after_tool`: changes to content, details and
//                           isError are chained, then applied (a replaced
//                           content keeps only its text). A handler that
//                           throws is reported and skipped, as in pi.
//   on("before_agent_start") `before_model`, once per run: text a handler
//                           adds around `event.systemPrompt` becomes a note
//                           on the request's latest user turn (yoagent never
//                           rewrites the system prompt — the prompt cache
//                           depends on it). A handler that throws, replaces
//                           the prompt, returns `message` or changes
//                           `systemPromptOptions` is reported and skipped;
//                           the others still count.
//   on("session_start")     fired once, when the adapter starts;
//   on("session_shutdown")  when it unloads.
//
// What does not map, and is reported when an extension registers it (a
// warning, or a load failure with `strict: true`): every other event —
// `context` and `message_end` rewrite the conversation, the boundary events
// continue it, the session events steer pi's session tree — commands,
// shortcuts, flags and renderers, which belong to the host app, and model
// providers and MCP servers. Runtime actions (`pi.sendMessage`,
// `pi.setActiveTools`, ...) throw pi's own "not initialized" error.
// `ctx.hasUI` is false and `ctx.ui` behaves as in pi's print mode: `confirm`
// answers false, `select` and `input` nothing, so a policy that would ask
// the user denies instead.

import { definePlugin } from '@arcships/rutis'
import type { Cancellable, ToolCall, ToolOutput, ToolResult, ToolSpec, Yoagent } from '../yoagent.d.ts'

export interface Config {
  /** The handler's name in the bridge (unique across the host's plugins). */
  name?: string
  /** pi extension files (or directories with an `index.ts`), loaded in order. */
  extensions: string[]
  /** The project directory extensions see as `ctx.cwd` (default: the runtime's). */
  cwd?: string
  /** yoagent tool name → the pi tool name policies see (merged over TOOL_NAMES). */
  toolNames?: Record<string, string>
  /** Fail loading when an extension registers something that does not map. */
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

type Args = Record<string, unknown>

/**
 * yoagent ↔ pi argument shapes, where they differ, keyed by the yoagent
 * built-in: only those calls are translated (a pi tool that is itself named
 * `edit` keeps its arguments as they are).
 */
const ARGS: Record<string, { toPi(args: Args): Args; fromPi(input: Args): Args }> = {
  edit_file: {
    toPi: ({ old_text, new_text, ...rest }) => ({ ...rest, edits: [{ oldText: old_text, newText: new_text }] }),
    fromPi: ({ edits, ...rest }) => {
      const list = edits as { oldText?: unknown; newText?: unknown }[] | undefined
      if (!Array.isArray(list) || list.length !== 1) {
        throw new Error("a pi extension rewrote `edits` into something other than one edit, which yoagent's edit_file cannot run")
      }
      return { ...rest, old_text: list[0].oldText, new_text: list[0].newText }
    },
  },
  search: {
    toPi: ({ include, case_sensitive, ...rest }) => ({
      ...rest,
      ...(include === undefined ? {} : { glob: include }),
      ...(case_sensitive === undefined ? {} : { ignoreCase: !case_sensitive }),
    }),
    fromPi: ({ glob, ignoreCase, ...rest }) => ({
      ...rest,
      ...(glob === undefined ? {} : { include: glob }),
      ...(ignoreCase === undefined ? {} : { case_sensitive: !ignoreCase }),
    }),
  },
  list_files: {
    // pi's `find` requires a pattern; yoagent's list_files has an optional one.
    toPi: ({ pattern, ...rest }) => ({ ...rest, pattern: pattern ?? '*' }),
    fromPi: ({ pattern, ...rest }) => (pattern === '*' ? rest : { ...rest, pattern }),
  },
}

/** What `before_agent_start` handlers see as `event.systemPrompt`. */
const PROMPT_MARK = '\u0000yoagent-system-prompt\u0000'

/** The events this adapter fires; registering any other is reported. */
const MAPPED_EVENTS = new Set(['tool_call', 'tool_result', 'before_agent_start', 'session_start', 'session_shutdown'])

/** Any theme call returns its text unstyled (`theme.fg('dim', text)` → text). */
const PLAIN_THEME = new Proxy({} as Record<string, unknown>, {
  get: () => (...args: unknown[]) => args[args.length - 1],
})

/** pi's print-mode UI: nothing to show, no one to ask. */
const NO_UI = new Proxy(
  {
    select: async () => undefined,
    confirm: async () => false,
    input: async () => undefined,
    editor: async () => undefined,
    custom: async () => undefined,
    notify: (message: string, level?: string) => console.warn(`[pi ${level ?? 'info'}] ${message}`),
    getEditorText: () => '',
    getAllThemes: () => [],
    getTheme: () => undefined,
    getToolsExpanded: () => false,
    setTheme: () => ({ success: false, error: 'UI not available' }),
    theme: PLAIN_THEME,
  } as Record<string, unknown>,
  {
    // Every other UI call (setStatus, setWidget, ...) does nothing, as in pi.
    get: (target, key) => (key in target ? target[key as string] : () => undefined),
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
  mcpServers?: { list(): { name: string; extensionPath?: string }[] }
}

/** pi's loader, by file: the package exports only the discovering variant, which also loads ~/.pi. */
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
      strict: { type: 'boolean' },
    },
  },
  async apply(ctx, config) {
    const yoagent = ctx.use<Yoagent>('yoagent')
    const cwd = config.cwd ?? process.cwd()
    const names: Record<string, string> = { ...TOOL_NAMES, ...config.toolNames }
    const builtinFor = new Map(Object.entries(names).map(([yo, pi]) => [pi, yo]))

    const { SessionManager } = await import('@earendil-works/pi-coding-agent')
    const { validateToolArguments } = await import('@earendil-works/pi-ai')
    const loaded = await (await piLoader()).loadExtensions(config.extensions, cwd)
    if (loaded.errors.length > 0) {
      throw new Error(`pi extensions failed to load: ${loaded.errors.map((e) => `${e.path}: ${e.error}`).join('; ')}`)
    }
    for (const w of loaded.warnings ?? []) console.warn(`[pi] ${short(w.path)}: ${w.warning}`)
    const extensions = loaded.extensions

    const report = (message: string) => {
      if (config.strict) throw new Error(`pi extensions use what does not map — ${message}`)
      console.warn(`[pi] ${message}`)
    }
    const unmapped: string[] = []
    for (const ext of extensions) {
      const what = [
        ...[...ext.handlers.keys()].filter((e) => !MAPPED_EVENTS.has(e)).map((e) => `event "${e}"`),
        ...[...ext.commands.keys()].map((c) => `command /${c}`),
        ...[...ext.shortcuts.keys()].map((s) => `shortcut ${s}`),
        ...[...ext.flags.keys()].map((f) => `flag --${f}`),
        ...(ext.messageRenderers.size + (ext.toolRenderers?.length ?? 0) + (ext.entryRenderers?.size ?? 0) > 0
          ? ['renderers']
          : []),
        ...(loaded.runtime.pendingProviderRegistrations ?? [])
          .filter((p) => p.extensionPath === ext.path)
          .map((p) => `model provider ${p.name}`),
        ...(loaded.runtime.mcpServers?.list() ?? [])
          .filter((s) => s.extensionPath === ext.path)
          .map((s) => `MCP server ${s.name}`),
      ]
      if (what.length > 0) unmapped.push(`${short(ext.path)}: ${what.join(', ')}`)
    }
    if (unmapped.length > 0) report(`not available in yoagent, ignored: ${unmapped.join('; ')}`)

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

    const fire = async (event: string, payload: unknown) => {
      for (const { ext, fn } of handlers(event)) {
        try {
          await fn(payload, context())
        } catch (error) {
          console.warn(`[pi] ${short(ext.path)} ${event}: ${error}`)
        }
      }
    }

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
    /** The ones pi would activate, so the ones the model is offered. */
    const offered = () =>
      new Map(
        [...registered()].filter(([, tool]) => {
          const exposure = tool.exposure ?? 'direct'
          return (exposure === 'direct' || exposure === 'model-only') && tool.defaultActive !== false
        }),
      )

    /** Overrides of yoagent tools already warned about. */
    const shadowWarned = new Set<string>()

    // Notes from `before_agent_start`, computed once per run.
    const notes = new Map<string, Promise<string | undefined>>()

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
            | { systemPrompt?: string; message?: unknown }
            | undefined
          if (result?.message !== undefined) {
            throw new Error('returned a message, which yoagent cannot inject')
          }
          if (result?.systemPrompt !== undefined) {
            if (!result.systemPrompt.includes(PROMPT_MARK)) {
              throw new Error('replaced the system prompt; yoagent only takes text added to it')
            }
            systemPrompt = result.systemPrompt
          }
        } catch (error) {
          // As in pi: one handler's failure is reported, the others still count.
          console.warn(`[pi] ${short(ext.path)} before_agent_start, skipped: ${error instanceof Error ? error.message : error}`)
        }
      }
      const added = systemPrompt.split(PROMPT_MARK).map((part) => part.trim()).filter(Boolean)
      return added.length > 0 ? added.join('\n\n') : undefined
    }

    // Before registering: tools an extension adds at session start are offered from the first run.
    await fire('session_start', { type: 'session_start', reason: 'startup' })
    ctx.effect(() => fire('session_shutdown', { type: 'session_shutdown', reason: 'quit' }))

    ctx.effect(
      yoagent.register(config.name ?? 'pi-extensions', {
        async tools(): Promise<ToolSpec[]> {
          const tools = offered()
          for (const name of tools.keys()) {
            if (name in names && !shadowWarned.has(name)) {
              shadowWarned.add(name)
              report(
                `pi tool "${name}" is named like yoagent's own tool: if the host also installs that one, yoagent's wins and the pi tool never runs — leave it out`,
              )
            }
          }
          return [...tools.values()].map((tool) => ({
            name: tool.name,
            label: tool.label ?? null,
            description: [tool.description ?? '', ...(tool.promptGuidelines ?? []).map((g) => `- ${g}`)]
              .filter(Boolean)
              .join('\n'),
            parameters: tool.parameters,
          }))
        },

        async call_tool(call: ToolCall & Cancellable): Promise<ToolResult> {
          const tool = offered().get(call.tool)
          if (!tool) return { text: `pi tool ${call.tool} is no longer registered`, is_error: true }
          try {
            // Prepared in before_tool; validated again here, in case a later handler rewrote them.
            const params = validateToolArguments(tool as never, { name: tool.name, arguments: call.args } as never)
            const out = await tool.execute(call.call_id, params, call.signal, undefined, toolContext(call.signal))
            return { text: text(out.content), details: out.details ?? null, is_error: out.isError === true }
          } catch (error) {
            return { text: error instanceof Error ? error.message : String(error), is_error: true }
          }
        },

        async before_tool(call: ToolCall & Cancellable) {
          const piTools = offered()
          const own = piTools.get(call.tool)
          // A yoagent built-in whose pi counterpart an extension overrides: the model must use the override.
          const counterpart = names[call.tool]
          if (!own && counterpart && counterpart !== call.tool && piTools.has(counterpart)) {
            return { deny: `a pi extension replaces this tool with "${counterpart}"; call "${counterpart}" instead` }
          }
          let input: Args
          let original: Args = call.args
          if (own) {
            // As pi's agent loop: prepare, validate, then the policies judge the validated arguments.
            try {
              const prepared = own.prepareArguments ? (own.prepareArguments(call.args) as Args) : call.args
              input = validateToolArguments(own as never, { name: own.name, arguments: prepared } as never) as Args
            } catch (error) {
              return { deny: error instanceof Error ? error.message : String(error) }
            }
          } else {
            const shape = ARGS[call.tool]
            original = shape ? shape.toPi(call.args) : call.args
            input = structuredClone(original)
          }
          const toolName = own ? call.tool : (counterpart ?? call.tool)
          const before = JSON.stringify(input)
          for (const { ext, fn } of handlers('tool_call')) {
            const event = { type: 'tool_call', toolCallId: call.call_id, toolName, input }
            let result: { block?: boolean; reason?: string } | undefined
            try {
              result = (await fn(event, context(call.signal))) as typeof result
            } catch (error) {
              // As in pi: a failing tool_call handler blocks the call.
              return { deny: `pi extension ${short(ext.path)} failed: ${error}` }
            }
            if (result?.block) return { deny: result.reason ?? `blocked by pi extension ${short(ext.path)}` }
          }
          if (own) {
            // Prepared or coerced arguments count as a rewrite too.
            return JSON.stringify(input) === JSON.stringify(call.args) ? undefined : { args: input }
          }
          if (JSON.stringify(input) === before) return
          try {
            const shape = ARGS[call.tool]
            return { args: shape ? shape.fromPi(input) : input }
          } catch (error) {
            return { deny: String(error) }
          }
        },

        async after_tool(call: ToolCall & Cancellable, output: ToolOutput) {
          const editors = handlers('tool_result')
          if (editors.length === 0) return
          const own = offered().has(call.tool)
          const shape = own ? undefined : ARGS[call.tool]
          const event = {
            type: 'tool_result',
            toolCallId: call.call_id,
            toolName: own ? call.tool : (names[call.tool] ?? call.tool),
            input: shape ? shape.toPi(call.args) : call.args,
            // yoagent's text and image blocks have pi's shape.
            content: output.content as Block[],
            details: output.details,
            isError: output.is_error,
          }
          const changed = { content: false, details: false, isError: false }
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
              // As in pi: a failing tool_result handler is reported, the chain goes on.
              console.warn(`[pi] ${short(ext.path)} tool_result: ${error}`)
            }
          }
          if (!changed.content && !changed.details && !changed.isError) return
          return {
            // Only a replaced content is sent back, as text: yoagent's edit replaces every block.
            ...(changed.content ? { text: text(event.content) } : {}),
            ...(changed.details ? { details: event.details ?? null } : {}),
            ...(changed.isError ? { is_error: event.isError } : {}),
          }
        },

        async before_model(turn) {
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
        },
      }),
    )
  },
})
