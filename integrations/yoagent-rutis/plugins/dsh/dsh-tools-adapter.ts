// dsh tools as yoagent tools: every tool in a DeepSeek Harness (dsh) tool
// registry — the `tools` service of `@deepseek-ai/dsh-tools` — offered to
// yoagent agents through the yoagent-rutis bridge, plus a short turn note
// carrying the system-prompt sections dsh plugins wrote.
//
// dsh plugins are Cordis plugins; rutis's Node runtime runs them unchanged
// in its Cordis context. Load, as rutis-loader rows of one Node runtime
// whose package.json is this directory's:
//
//   @deepseek-ai/dsh-system-prompt   (the `systemPrompt` service)
//   @deepseek-ai/dsh-tools           (the `tools` service)
//   <your dsh tool plugins>          (e.g. dsh-free-search, with @deepseek-ai/dsh-web)
//   <this file>                      (the adapter)
//
// with `yoagent` shared in the loader's catalog. `examples/dsh_tools.rs` is a
// complete host.
//
// What it does, per hook:
//   tools        `tools.schemas()` (the global view) → {name, description, parameters}
//   call_tool    `tools.execute({callId, name, arguments, signal})`, with the
//                bridge's cancel handle as dsh's `signal`: cancelling the
//                yoagent run aborts the dsh call. `isError` → an error tool
//                result; text blocks are joined, other blocks named.
//   before_model the sections dsh plugins added to `systemPrompt` (the
//                harness identity and persona slots left out), rendered and
//                capped, as a note on the request's latest user turn.

import { definePlugin } from '@arcships/rutis'
import { renderPrompt } from '@deepseek-ai/dsh-system-prompt'
import type { Cancellable, ToolCall, ToolResult, ToolSpec, Yoagent } from '../yoagent.d.ts'

/** The slice of `@deepseek-ai/dsh-tools`' `ToolRuntime` this adapter uses. */
interface DshTools {
  schemas(): { name: string; description?: string; parameters?: Record<string, unknown> }[]
  execute(exec: {
    callId: string
    name: string
    arguments: unknown
    signal: AbortSignal
  }): Promise<{
    isError: boolean
    content?: { type: string; text?: string }[]
    error?: { message?: string; info?: { code?: string } }
  }>
}

/** The slice of `@deepseek-ai/dsh-system-prompt`'s `SystemPrompt` this adapter uses. */
interface DshSystemPrompt {
  assemble(context?: { signal?: AbortSignal }): Promise<{
    sections: { name: string; text: string; interpolate?: boolean }[]
    contexts: unknown[]
    tools: unknown[]
    variables: Record<string, string | undefined>
  }>
}

export interface Config {
  /** The handler's name in the bridge (unique across the host's plugins). */
  name?: string
  /** Offer only these dsh tools (default: every tool in the registry). */
  tools?: string[]
  /** Cap on the turn note's length, in characters (0: no note). */
  maxNoteChars?: number
}

/** Sections `dsh-system-prompt` registers itself: the harness's, not a plugin's. */
const OWN_SECTIONS = new Set([
  'harness:identity',
  'deployment:persona-prefix',
  'deployment:persona-suffix',
])

const EMPTY_SCHEMA = { type: 'object', properties: {} }

export default definePlugin<Config>({
  inject: ['tools', 'systemPrompt', 'yoagent'],
  config: {
    type: 'object',
    properties: {
      name: { type: 'string' },
      tools: { type: 'array', items: { type: 'string' } },
      maxNoteChars: { type: 'integer', minimum: 0 },
    },
  },
  apply(ctx, config) {
    const dsh = ctx.use<DshTools>('tools')
    const prompt = ctx.use<DshSystemPrompt>('systemPrompt')
    const yoagent = ctx.use<Yoagent>('yoagent')
    const only = config?.tools ? new Set(config.tools) : undefined
    const maxNote = config?.maxNoteChars ?? 2000

    ctx.effect(
      yoagent.register(config?.name ?? 'dsh-tools', {
        async tools(): Promise<ToolSpec[]> {
          return dsh
            .schemas()
            .filter((schema) => !only || only.has(schema.name))
            .map((schema) => ({
              name: schema.name,
              description: schema.description ?? '',
              parameters: schema.parameters ?? EMPTY_SCHEMA,
            }))
        },

        async call_tool(call: ToolCall & Cancellable): Promise<ToolResult> {
          const out = await dsh.execute({
            callId: `yoagent:${call.call_id}`,
            name: call.tool,
            arguments: call.args,
            signal: call.signal,
          })
          const text = (out.content ?? [])
            .map((block) => (block.type === 'text' ? (block.text ?? '') : `[${block.type} block]`))
            .join('\n')
          if (out.isError) {
            return { text: text || out.error?.message || 'the dsh tool failed', is_error: true }
          }
          return { text }
        },

        async before_model(turn: Cancellable) {
          if (maxNote === 0) return
          const assembly = await prompt.assemble({ signal: turn.signal })
          const parts: string[] = []
          for (const section of assembly.sections) {
            if (OWN_SECTIONS.has(section.name)) continue
            let text: string
            try {
              // One section at a time: a section whose variables are not
              // set here (the dsh loop supplies `model`, `cwd`) is skipped,
              // not the whole note.
              text = renderPrompt({ ...assembly, sections: [section], contexts: [], tools: [] })
            } catch {
              continue
            }
            if (text.trim()) parts.push(text.trim())
          }
          if (parts.length === 0) return
          let note = `[Guidance from dsh plugins]\n${parts.join('\n\n')}`
          if (note.length > maxNote) note = `${note.slice(0, Math.max(0, maxNote - 1))}…`
          return { note }
        },
      }),
    )
  },
})
