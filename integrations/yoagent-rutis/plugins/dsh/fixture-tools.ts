// A test fixture: a tiny dsh tool plugin (a plain Cordis plugin, as dsh
// plugins are) built with `@deepseek-ai/dsh-tools`' `defineTool`, with no
// network. The end-to-end test `tests/dsh_test.rs` loads it next to
// `dsh-tools-adapter.ts`. Not for production use.
//
//   fixture_echo  {text}  → "echo: <text>"; presents its call and result
//                           (dsh's card vocabulary, read by the adapter)
//   fixture_guarded {}    → "guarded ran", behind a `tools/pre-execute`
//                           policy that asks before every call
//   fixture_fail  {why}   → throws: dsh reports an `isError` result
//   fixture_dot   {}      → a text block, an image block referring to the
//                           attachment `fixture-dot` (see fixture-attachments.ts),
//                           an offloaded one and an oversize one
//   fixture_slow  {}      → writes "started" to `config.abortFile`, waits for
//                           `exec.signal` to abort (60 s at most), then
//                           writes how it ended there
//
// It also adds a system-prompt section, `fixture:guidance`.

import { writeFileSync } from 'node:fs'
import { defineTool } from '@deepseek-ai/dsh-tools'

export const name = 'yoagent-dsh-fixture'
export const inject = ['tools', 'systemPrompt']

export interface Config {
  /** Where `fixture_slow` reports how it ended. */
  abortFile?: string
}

const TEXT = { type: 'string' } as const
const TEXT_OUTPUT = {
  schema: TEXT,
  render: (_args: unknown, value: string) => [{ type: 'text' as const, text: value }],
}

// The Cordis context is used untyped: Cordis types are not needed at runtime.
export function apply(ctx: any, config: Config | undefined) {
  const tools = [
    defineTool({
      name: 'fixture_echo',
      description: 'Echo a text back.',
      parameters: { text: { type: 'string', description: 'The text to echo.', required: true } },
      output: TEXT_OUTPUT,
      async execute(args: { text: string }) {
        return `echo: ${args.text}`
      },
      // Shown as a shell command would be.
      presentCall: (args: { text: string }) => ({ card: 'terminal' as const, title: `echo ${args.text}`, description: 'Echo a text back' }),
      presentResult: (_args: unknown, result: { content: { type: string; text?: string }[] }) => ({
        card: 'terminal' as const,
        output: result.content.map((block) => block.text ?? '').join(''),
        exitCode: 0,
      }),
    }),
    defineTool({
      name: 'fixture_guarded',
      description: 'Runs only once someone approves it.',
      parameters: {},
      output: TEXT_OUTPUT,
      async execute() {
        return 'guarded ran'
      },
    }),
    defineTool({
      name: 'fixture_fail',
      description: 'Always fails, with the reason given.',
      parameters: { why: { type: 'string', required: true } },
      output: TEXT_OUTPUT,
      async execute(args: { why: string }): Promise<string> {
        throw new Error(`fixture failure: ${args.why}`)
      },
    }),
    defineTool({
      name: 'fixture_dot',
      description: 'Shows a picture of a dot.',
      parameters: {},
      output: {
        schema: TEXT,
        render: () => [
          { type: 'text' as const, text: 'a dot' },
          {
            type: 'image' as const,
            attachment: { attachmentId: 'fixture-dot', mediaType: 'image/png', bytes: 70, width: 1, height: 1 },
          },
          // dsh decided to send this one as text.
          {
            type: 'image' as const,
            attachment: { attachmentId: 'fixture-dot', mediaType: 'image/png', bytes: 70, width: 1, height: 1, name: 'offloaded.png' },
            offloaded: true as const,
          },
          // Over the adapter's provider-safe limit.
          {
            type: 'image' as const,
            attachment: { attachmentId: 'fixture-huge', mediaType: 'image/png', bytes: 9_000_000, width: 9000, height: 9000, name: 'huge.png' },
          },
        ],
      },
      async execute() {
        return 'a dot'
      },
    }),
    defineTool({
      name: 'fixture_slow',
      description: 'Waits until it is cancelled.',
      parameters: {},
      output: TEXT_OUTPUT,
      async execute(_args: unknown, exec: { signal: AbortSignal }) {
        const { signal } = exec
        if (config?.abortFile) writeFileSync(config.abortFile, 'started')
        await new Promise<void>((resolve) => {
          if (signal.aborted) return resolve()
          signal.addEventListener('abort', () => resolve(), { once: true })
          setTimeout(resolve, 60_000)
        })
        const how = signal.aborted ? `aborted: ${signal.reason?.name ?? signal.reason}` : 'not aborted'
        if (config?.abortFile) writeFileSync(config.abortFile, how)
        return how
      },
    }),
  ]
  for (const tool of tools) ctx.effect(() => ctx.tools.register(tool), `fixture: ${tool.name}`)
  // A dsh tool policy: ask before every fixture_guarded call.
  ctx.on('tools/pre-execute', async (exec: { name: string }, next: () => Promise<unknown>) =>
    exec.name === 'fixture_guarded' ? { kind: 'ask', reason: 'fixture_guarded needs a yes' } : next(),
  )
  ctx.effect(
    () =>
      ctx.systemPrompt.section({
        name: 'fixture:guidance',
        order: 4000,
        text: 'Fixture guidance: prefer fixture_echo for echoing.',
      }),
    'fixture: prompt section',
  )
}
