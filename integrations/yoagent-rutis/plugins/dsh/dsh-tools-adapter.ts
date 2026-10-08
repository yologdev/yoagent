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
//                result. Text blocks stay text; an image block (a reference
//                into dsh's attachment store) becomes a yoagent image, read
//                with the `attachments` service when one is loaded (looked up
//                per image, not injected: the adapter runs without it); with
//                none, or when a read fails, the image is a text placeholder.
//                Other blocks are named.
//   before_model the sections dsh plugins added to `systemPrompt` (the
//                harness identity and persona slots left out), rendered and
//                capped, as a note on the request's latest user turn.

import { definePlugin } from '@arcships/rutis'
import { renderPrompt } from '@deepseek-ai/dsh-system-prompt'
import type { Cancellable, ContentBlock, ToolCall, ToolResult, ToolSpec, Yoagent } from '../yoagent.d.ts'

/** The slice of `@deepseek-ai/dsh-attachment`'s `AttachmentStore` this adapter uses. */
interface DshAttachments {
  readImage(
    ref: { attachmentId: string; mediaType: string },
    signal?: AbortSignal,
  ): Promise<{ ref: { mediaType: string }; data: Uint8Array }>
}

/** A dsh content block as a tool result carries it. */
type DshBlock = { type: string; text?: string; attachment?: { attachmentId: string; mediaType: string; name?: string } }

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
    content?: DshBlock[]
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
    /** dsh's attachment store, when one is loaded: optional, so looked up per use rather than injected. */
    const attachments = (): DshAttachments | undefined => {
      try {
        return ctx.use<DshAttachments>('attachments')
      } catch {
        return undefined
      }
    }
    const maxNote = config?.maxNoteChars ?? 2000

    /** An image reference as a yoagent image: its bytes from dsh's attachment store. */
    const image = async (
      ref: { attachmentId: string; mediaType: string; name?: string },
      signal: AbortSignal,
    ): Promise<ContentBlock> => {
      const label = `[image${ref.name ? ` ${ref.name}` : ''}: not available here]`
      const store = attachments()
      if (!store) return { type: 'text', text: label }
      try {
        const stored = await store.readImage(ref, signal)
        return { type: 'image', data: Buffer.from(stored.data).toString('base64'), mimeType: stored.ref.mediaType }
      } catch (error) {
        if (signal.aborted) throw error
        console.warn(`[dsh] image ${ref.attachmentId} could not be read: ${error}`)
        return { type: 'text', text: label }
      }
    }

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
          const content: ContentBlock[] = []
          for (const block of out.content ?? []) {
            if (block.type === 'text') {
              content.push({ type: 'text', text: block.text ?? '' })
            } else if (block.type === 'image' && block.attachment) {
              content.push(await image(block.attachment, call.signal))
            } else {
              content.push({ type: 'text', text: `[${block.type} block]` })
            }
          }
          if (out.isError) {
            const text = content.flatMap((b) => (b.type === 'text' ? [b.text] : [])).join('\n')
            return { text: text || out.error?.message || 'the dsh tool failed', is_error: true }
          }
          return { content }
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
          // Cut by code points, not UTF-16 units: a lone surrogate would fail the
          // host's JSON decoding and close the whole runtime session.
          const chars = [...note]
          if (chars.length > maxNote) note = `${chars.slice(0, Math.max(0, maxNote - 1)).join('')}…`
          return { note }
        },
      }),
    )
  },
})
