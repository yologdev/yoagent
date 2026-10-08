// A pi extension for the adapter's tests (`tests/pi_test.rs`): written as
// any pi extension is, against pi's own API, with no knowledge of yoagent.
// No network; the slow tool writes `slow.txt` and `session_shutdown` writes
// `shutdown.txt` in `ctx.cwd`.

import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { defineTool, type ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { Type } from 'typebox'

const echo = defineTool({
  name: 'pi_echo',
  label: 'Echo',
  description: 'Echo the text back.',
  promptGuidelines: ['Use pi_echo to repeat text exactly.'],
  parameters: Type.Object({ text: Type.String() }),
  async execute(_id, params) {
    return { content: [{ type: 'text', text: `pi echo: ${params.text}` }], details: { length: params.text.length } }
  },
})

export default function (pi: ExtensionAPI) {
  pi.registerTool(echo)

  pi.registerTool({
    name: 'pi_fail',
    label: 'Fail',
    description: 'Always fails.',
    parameters: Type.Object({ why: Type.String() }),
    async execute(_id, params) {
      throw new Error(`pi failure: ${params.why}`)
    },
  })

  pi.registerTool({
    name: 'pi_slow',
    label: 'Slow',
    description: 'Waits until cancelled.',
    parameters: Type.Object({}),
    async execute(_id, _params, signal, _onUpdate, ctx) {
      const file = join(ctx.cwd, 'slow.txt')
      writeFileSync(file, 'started')
      await new Promise<void>((resolve) => {
        if (signal?.aborted) return resolve()
        signal?.addEventListener('abort', () => resolve(), { once: true })
      })
      writeFileSync(file, `aborted: ${signal?.reason?.name ?? signal?.reason}`)
      return { content: [{ type: 'text', text: 'cancelled' }], details: undefined }
    },
  })

  // A picture: text and an image block, pi's shape.
  pi.registerTool({
    name: 'pi_picture',
    label: 'Picture',
    description: 'Shows a picture of a dot.',
    parameters: Type.Object({}),
    async execute() {
      return {
        content: [
          { type: 'text', text: 'a dot' },
          {
            type: 'image',
            data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==',
            mimeType: 'image/png',
          },
        ],
        details: undefined,
      }
    },
  })

  // A tool registered when the session starts (pi's dynamic-tools pattern).
  pi.on('session_start', () => {
    pi.registerTool({
      name: 'pi_dynamic',
      label: 'Dynamic',
      description: 'Registered at session start.',
      parameters: Type.Object({}),
      async execute() {
        return { content: [{ type: 'text', text: 'dynamic ok' }], details: undefined }
      },
    })
  })

  // pi's protected-paths pattern: block writes to .env.
  pi.on('tool_call', async (event) => {
    if ((event.toolName === 'write' || event.toolName === 'edit') && String(event.input.path).includes('.env')) {
      return { block: true, reason: `Path "${event.input.path}" is protected` }
    }
    // Rewrite in place, as pi documents.
    if (event.toolName === 'bash' && event.input.command === 'echo original') {
      event.input.command = 'echo rewritten'
    }
    if (event.toolName === 'edit') {
      const edits = event.input.edits as { oldText: string; newText: string }[]
      edits[0].newText = edits[0].newText.toUpperCase()
      // Two edits: yoagent's edit_file runs one, so the call is denied.
      if (String(event.input.path).endsWith('multi.txt')) edits.push({ oldText: 'x', newText: 'y' })
    }
    return undefined
  })

  // A content edit that keeps the picture and adds a caption.
  pi.on('tool_result', async (event) =>
    event.toolName === 'pi_picture' ? { content: [...event.content, { type: 'text', text: 'captioned' }] } : undefined,
  )

  // Redaction: results are chained edits.
  pi.on('tool_result', async (event) => {
    const text = event.content.map((block) => (block.type === 'text' ? block.text : '')).join('')
    if (text.includes('SECRET')) {
      return { content: [{ type: 'text', text: text.replaceAll('SECRET', '[redacted]') }] }
    }
    return undefined
  })

  // Guidance added to the system prompt.
  pi.on('before_agent_start', async (event) => ({
    systemPrompt: `${event.systemPrompt}\n\nFixture rules: answer in one line.`,
  }))

  // When the adapter unloads.
  pi.on('session_shutdown', (_event, ctx) => {
    writeFileSync(join(ctx.cwd, 'shutdown.txt'), 'bye')
  })

  // App-level: reported by the adapter as not available.
  pi.registerCommand('fixture', { description: 'A command', handler: async () => {} })
}
