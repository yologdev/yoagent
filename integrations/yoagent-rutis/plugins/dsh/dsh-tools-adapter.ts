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
//                per image, not injected: the adapter runs without it). With
//                none, when a read fails, when dsh marked the image
//                `offloaded`, or when it is over MAX_IMAGE_BYTES (provider-safe:
//                Anthropic takes 5 MB base64), the image is a text
//                placeholder. Other blocks are named. An error result's
//                images are not read.
//                The tool's own presenters (`presentCall` / `presentResult`,
//                dsh's card vocabulary: generic, terminal, diff, search,
//                read, web) go along with a successful result as
//                `details.view = {call?, result?}`, for a frontend to draw:
//                content blocks as text (others as `[type block]`), over
//                100k JSON characters left out. An error result carries no
//                card (the bridge reports it by its text).
//   before_model the sections dsh plugins added to `systemPrompt` (the
//                harness identity and persona slots left out), rendered and
//                capped, as a note on the request's latest user turn.
//
// dsh's dialogs (an approval its policy asks for, `ask_user_question`) are
// `host-dialogs.ts`'s, loaded when the host provides a `ui` service.

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
type DshBlock = {
  type: string
  text?: string
  attachment?: { attachmentId: string; mediaType: string; name?: string; bytes?: number }
  offloaded?: true
}

/** Largest image sent as an image: under Anthropic's 5 MB once base64-encoded. */
const MAX_IMAGE_BYTES = 3_750_000

/** A dsh tool definition's presenters (`@deepseek-ai/dsh-tools/presentation`). */
interface DshPresenters {
  presentCall?(args: unknown): unknown
  presentResult?(args: unknown, result: { content: DshBlock[]; isError: boolean; meta?: unknown }): unknown
}

/** The slice of `@deepseek-ai/dsh-tools`' `ToolRuntime` this adapter uses. */
interface DshTools {
  schemas(): { name: string; description?: string; parameters?: Record<string, unknown> }[]
  get?(name: string): DshPresenters | undefined
  execute(exec: {
    callId: string
    name: string
    arguments: unknown
    signal: AbortSignal
  }): Promise<{
    isError: boolean
    content?: DshBlock[]
    meta?: unknown
    error?: { message?: string; info?: { code?: string } }
  }>
}

/** Largest `details.view`, as JSON characters: a bigger one is left out. */
const MAX_VIEW_CHARS = 100_000

/**
 * A presenter's view as plain JSON the host can parse: content blocks reduced
 * to text (an image is an attachment reference, not data), strings made
 * well-formed (a lone surrogate fails the host's JSON decoding and closes the
 * runtime session).
 */
const plain = (value: unknown): unknown => {
  if (typeof value === 'string') return value.toWellFormed()
  // JSON has no BigInt: a presenter's big number goes as its digits.
  if (typeof value === 'bigint') return value.toString()
  if (Array.isArray(value)) return value.map(plain)
  if (value === null || typeof value !== 'object') return typeof value === 'number' && !Number.isFinite(value) ? null : value
  // A Date (or anything with its own JSON form) as JSON would write it.
  if (typeof (value as { toJSON?: unknown }).toJSON === 'function') return plain((value as { toJSON(): unknown }).toJSON())
  const out: Record<string, unknown> = {}
  for (const [raw, field] of Object.entries(value)) {
    // Keys too: a lone surrogate in a key fails the host's decoding just the same.
    const key = raw.toWellFormed()
    if (key === 'content' && Array.isArray(field)) {
      out.content = field.map((block: DshBlock) =>
        block?.type === 'text' ? { type: 'text', text: String(block.text ?? '').toWellFormed() } : { type: 'text', text: `[${block?.type} block]` },
      )
    } else if (field !== undefined && typeof field !== 'function') {
      out[key] = plain(field)
    }
  }
  return out
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
    const say = (level: 'warn' | 'debug', message: string) => {
      // On a host without `log`, rutis's stand-in throws: fall back either way.
      try {
        yoagent.log?.(level, message)?.catch(() => console.warn(message))
      } catch {
        console.warn(message)
      }
    }

    /** The tool's own presentation of this call, when it has presenters. */
    /** Tools whose presenters already failed once: warned once, then debug. */
    const presentFailed = new Set<string>()
    const presenterFailed = (what: string, error: unknown) => {
      const message = `[dsh] ${what} failed, so the call has no card: ${error}`
      if (presentFailed.has(what)) return say('debug', message)
      presentFailed.add(what)
      say('warn', message)
    }
    const view = (name: string, args: unknown, out: { isError: boolean; content?: DshBlock[]; meta?: unknown }) => {
      let tool: DshPresenters | undefined
      try {
        tool = dsh.get?.(name)
      } catch (error) {
        presenterFailed(`looking up ${name}`, error)
        return undefined
      }
      const views: { call?: unknown; result?: unknown } = {}
      try {
        const call = tool?.presentCall?.(args)
        if (call) views.call = plain(call)
      } catch (error) {
        presenterFailed(`${name}.presentCall`, error)
      }
      try {
        const result = tool?.presentResult?.(args, {
          content: out.content ?? [],
          isError: out.isError,
          ...(out.meta !== undefined ? { meta: out.meta } : {}),
        })
        if (result) views.result = plain(result)
      } catch (error) {
        presenterFailed(`${name}.presentResult`, error)
      }
      if (views.call === undefined && views.result === undefined) return undefined
      // Never fail a call that already ran over how it is shown.
      try {
        if (JSON.stringify(views).length > MAX_VIEW_CHARS) {
          say('debug', `[dsh] ${name}'s view is over ${MAX_VIEW_CHARS} characters: left out`)
          return undefined
        }
      } catch (error) {
        say('debug', `[dsh] ${name}'s view is not JSON: left out (${error})`)
        return undefined
      }
      return views
    }

    /** An image reference as a yoagent image: its bytes from dsh's attachment store. */
    const image = async (
      ref: { attachmentId: string; mediaType: string; name?: string; bytes?: number },
      offloaded: boolean,
      signal: AbortSignal,
    ): Promise<ContentBlock> => {
      const named = `image${ref.name ? ` ${ref.name}` : ''}`
      const label = `[${named}: not available here]`
      // dsh decided this image goes out as text; and an oversize one would fail the request.
      if (offloaded) return { type: 'text', text: `[${named}: offloaded]` }
      if ((ref.bytes ?? 0) > MAX_IMAGE_BYTES) return { type: 'text', text: `[${named}: too large to send]` }
      const store = attachments()
      if (!store) return { type: 'text', text: label }
      try {
        const stored = await store.readImage(ref, signal)
        if (stored.data.byteLength > MAX_IMAGE_BYTES) return { type: 'text', text: `[${named}: too large to send]` }
        return { type: 'image', data: Buffer.from(stored.data).toString('base64'), mimeType: stored.ref.mediaType }
      } catch (error) {
        if (signal.aborted) throw error
        say('warn', `[dsh] image ${ref.attachmentId} could not be read: ${error}`)
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
          if (out.isError) {
            const text = (out.content ?? [])
              .map((block) => (block.type === 'text' ? (block.text ?? '') : `[${block.type} block]`))
              .join('\n')
            // No card: the bridge reports an error result by its text alone.
            return { text: text || out.error?.message || 'the dsh tool failed', is_error: true }
          }
          const content: ContentBlock[] = []
          for (const block of out.content ?? []) {
            if (block.type === 'text') {
              content.push({ type: 'text', text: block.text ?? '' })
            } else if (block.type === 'image' && block.attachment) {
              content.push(await image(block.attachment, block.offloaded === true, call.signal))
            } else {
              content.push({ type: 'text', text: `[${block.type} block]` })
            }
          }
          const shown = view(call.tool, call.args, out)
          return { content, ...(shown ? { details: { view: shown } } : {}) }
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
