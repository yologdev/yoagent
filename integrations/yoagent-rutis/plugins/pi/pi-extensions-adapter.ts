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
// What maps, per pi API:
//   pi.registerTool         a yoagent tool (TypeBox schemas are JSON Schema);
//                           `execute` gets the bridge's cancel handle as its
//                           signal; a throw or `isError` is an error result.
//                           Tools registered later (in `session_start`, say)
//                           are offered from the next run on.
//   on("tool_call")         `before_tool`, for every call of the run (the
//                           agent's own tools too, under pi's names: see
//                           TOOL_NAMES): `{ block }` denies with its reason;
//                           changes to `event.input` rewrite the arguments.
//                           A handler that throws blocks, as in pi.
//   on("tool_result")       `after_tool`: changes to content, details and
//                           isError are chained, then applied.
//   on("before_agent_start") `before_model`, once per run: text a handler
//                           adds around `event.systemPrompt` becomes a note
//                           on the request's latest user turn (yoagent never
//                           rewrites the system prompt — the prompt cache
//                           depends on it). Replacing the prompt outright, or
//                           returning `message`, fails the hook.
//   on("session_start")     fired once, when the adapter starts;
//   on("session_shutdown")  when it unloads.
//
// What does not map, and is reported when an extension registers it (a
// warning, or a load failure with `strict: true`): every other event —
// `context` and `message_end` rewrite the conversation, the boundary events
// continue it, the session events steer pi's session tree — and commands,
// shortcuts, flags and renderers, which belong to the host app. Runtime
// actions (`pi.sendMessage`, `pi.setActiveTools`, ...) throw pi's own "not
// initialized" error. `ctx.hasUI` is false and `ctx.ui` behaves as in pi's
// print mode: `confirm` answers false, `select` and `input` nothing, so a
// policy that would ask the user denies instead.

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

/** yoagent ↔ pi argument shapes, where they differ. */
const ARGS: Record<string, { toPi(args: Args): Args; fromPi(input: Args): Args }> = {
  edit: {
    toPi: ({ old_text, new_text, ...rest }) => ({ ...rest, edits: [{ oldText: old_text, newText: new_text }] }),
    fromPi: ({ edits, ...rest }) => {
      const list = edits as { oldText?: unknown; newText?: unknown }[] | undefined
      if (!Array.isArray(list) || list.length !== 1) {
        throw new Error('a pi extension rewrote `edits` into something other than one edit, which yoagent\'s edit_file cannot run')
      }
      return { ...rest, old_text: list[0].oldText, new_text: list[0].newText }
    },
  },
  grep: {
    toPi: ({ include, ...rest }) => (include === undefined ? rest : { ...rest, glob: include }),
    fromPi: ({ glob, ...rest }) => (glob === undefined ? rest : { ...rest, include: glob }),
  },
}

/** What `before_agent_start` handlers see as `event.systemPrompt`. */
const PROMPT_MARK = '\u0000yoagent-system-prompt\u0000'

/** The events this adapter fires; registering any other is reported. */
const MAPPED_EVENTS = new Set(['tool_call', 'tool_result', 'before_agent_start', 'session_start', 'session_shutdown'])

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
  } as Record<string, unknown>,
  {
    // Every other UI call (setStatus, setWidget, ...) does nothing, as in pi.
    get: (target, key) => (key in target ? target[key as string] : () => undefined),
  },
)

interface PiTool {
  name: string
  label?: string
  description?: string
  promptGuidelines?: string[]
  parameters: Record<string, unknown>
  exposure?: string
  prepareArguments?: (args: unknown) => unknown
  execute(id: string, params: unknown, signal: AbortSignal | undefined, onUpdate: unknown, ctx: unknown): Promise<{
    content?: { type: string; text?: string }[]
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

/** pi's loader, by file: the package exports only the discovering variant, which also loads ~/.pi. */
async function piLoader(): Promise<{
  loadExtensions(paths: string[], cwd: string): Promise<{
    extensions: PiExtension[]
    errors: { path: string; error: string }[]
    warnings?: { path: string; warning: string }[]
  }>
}> {
  const index = import.meta.resolve('@earendil-works/pi-coding-agent')
  return import(new URL('./core/extensions/loader.js', index).href)
}

const short = (path: string) => path.split('/').pop() ?? path

function text(content: { type: string; text?: string }[] | undefined): string {
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
    const names = { ...TOOL_NAMES, ...config.toolNames }

    const { SessionManager } = await import('@earendil-works/pi-coding-agent')
    const loaded = await (await piLoader()).loadExtensions(config.extensions, cwd)
    if (loaded.errors.length > 0) {
      throw new Error(`pi extensions failed to load: ${loaded.errors.map((e) => `${e.path}: ${e.error}`).join('; ')}`)
    }
    for (const w of loaded.warnings ?? []) console.warn(`[pi] ${short(w.path)}: ${w.warning}`)
    const extensions = loaded.extensions

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
      ]
      if (what.length > 0) unmapped.push(`${short(ext.path)}: ${what.join(', ')}`)
    }
    if (unmapped.length > 0) {
      const message = `not available in yoagent, ignored: ${unmapped.join('; ')}`
      if (config.strict) throw new Error(`pi extensions use what does not map — ${message}`)
      console.warn(`[pi] ${message}`)
    }

    const sessionManager = SessionManager.inMemory(cwd)
    const context = (signal?: AbortSignal) => ({
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
      getSystemPrompt: () => '',
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

    /** The tools to offer: every registered tool the model is meant to see, the latest registration of a name winning. */
    const tools = () => {
      const byName = new Map<string, PiTool>()
      for (const ext of extensions) {
        for (const { definition } of ext.tools.values()) {
          const exposure = definition.exposure ?? 'direct'
          if (exposure === 'direct' || exposure === 'model-only') byName.set(definition.name, definition)
          else byName.delete(definition.name)
        }
      }
      return byName
    }

    // Notes from `before_agent_start`, computed once per run.
    const notes = new Map<string, Promise<string | undefined>>()

    const startNote = async (run: string, prompt: string, signal: AbortSignal) => {
      const before = handlers('before_agent_start')
      if (before.length === 0) return undefined
      let systemPrompt = PROMPT_MARK
      const options = new Proxy({} as Record<string, unknown>, {
        set: () => {
          throw new Error('changing systemPromptOptions is not available in yoagent; add text to systemPrompt instead')
        },
      })
      for (const { ext, fn } of before) {
        const event = { type: 'before_agent_start', prompt, systemPrompt, systemPromptOptions: options }
        const result = (await fn(event, context(signal))) as { systemPrompt?: string; message?: unknown } | undefined
        if (result?.message !== undefined) {
          throw new Error(`${short(ext.path)}: before_agent_start returned a message, which yoagent cannot inject`)
        }
        if (result?.systemPrompt !== undefined) {
          if (!result.systemPrompt.includes(PROMPT_MARK)) {
            throw new Error(
              `${short(ext.path)}: before_agent_start replaced the system prompt; yoagent only takes text added to it`,
            )
          }
          systemPrompt = result.systemPrompt
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
          return [...tools().values()].map((tool) => ({
            name: tool.name,
            label: tool.label ?? null,
            description: [tool.description ?? '', ...(tool.promptGuidelines ?? []).map((g) => `- ${g}`)]
              .filter(Boolean)
              .join('\n'),
            parameters: tool.parameters,
          }))
        },

        async call_tool(call: ToolCall & Cancellable): Promise<ToolResult> {
          const tool = tools().get(call.tool)
          if (!tool) return { text: `pi tool ${call.tool} is no longer registered`, is_error: true }
          const params = tool.prepareArguments ? tool.prepareArguments(call.args) : call.args
          try {
            const out = await tool.execute(call.call_id, params, call.signal, undefined, toolContext(call.signal))
            return { text: text(out.content), details: out.details ?? null, is_error: out.isError === true }
          } catch (error) {
            return { text: error instanceof Error ? error.message : String(error), is_error: true }
          }
        },

        async before_tool(call: ToolCall & Cancellable) {
          const policies = handlers('tool_call')
          if (policies.length === 0) return
          const toolName = names[call.tool] ?? call.tool
          const shape = ARGS[toolName]
          const original = shape ? shape.toPi(call.args) : call.args
          const input = structuredClone(original)
          for (const { ext, fn } of policies) {
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
          if (JSON.stringify(input) === JSON.stringify(original)) return
          try {
            return { args: shape ? shape.fromPi(input) : input }
          } catch (error) {
            return { deny: String(error) }
          }
        },

        async after_tool(call: ToolCall & Cancellable, output: ToolOutput) {
          const editors = handlers('tool_result')
          if (editors.length === 0) return
          const toolName = names[call.tool] ?? call.tool
          const shape = ARGS[toolName]
          const event = {
            type: 'tool_result',
            toolCallId: call.call_id,
            toolName,
            input: shape ? shape.toPi(call.args) : call.args,
            content: [{ type: 'text', text: output.text }],
            details: output.details,
            isError: output.is_error,
          }
          let changed = false
          for (const { ext, fn } of editors) {
            try {
              const result = (await fn(event, context(call.signal))) as
                | { content?: { type: string; text?: string }[]; details?: unknown; isError?: boolean }
                | undefined
              if (!result) continue
              if (result.content !== undefined) event.content = result.content as typeof event.content
              if (result.details !== undefined) event.details = result.details
              if (result.isError !== undefined) event.isError = result.isError
              changed = true
            } catch (error) {
              // As in pi: a failing tool_result handler is reported, the chain goes on.
              console.warn(`[pi] ${short(ext.path)} tool_result: ${error}`)
            }
          }
          if (!changed) return
          return { text: text(event.content), details: event.details ?? null, is_error: event.isError }
        },

        async before_model(turn) {
          let note = notes.get(turn.run_id)
          if (!note) {
            note = startNote(turn.run_id, turn.latest_user_text ?? '', turn.signal)
            notes.set(turn.run_id, note)
            // A failed attempt is not cached: the next request asks again.
            note.catch(() => notes.delete(turn.run_id))
          }
          const text = await note
          return text ? { note: text } : undefined
        },

        async finish(outcome) {
          notes.delete(outcome.run_id)
        },
      }),
    )

  },
})
