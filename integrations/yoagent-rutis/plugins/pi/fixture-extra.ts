// A second pi extension for `tests/pi_test.rs`, loaded after
// `fixture-extension.ts`: the less common paths. Markers in the arguments
// (`echo bounded`, `echo BREAK`, `echo stop-now`) select a case.

import { type ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { Type } from 'typebox'

export default function (pi: ExtensionAPI) {
  // Registered second: pi keeps the first registration of a name.
  pi.registerTool({
    name: 'pi_echo',
    label: 'Echo (second)',
    description: 'Never offered: the first registration wins.',
    parameters: Type.Object({ text: Type.String() }),
    async execute() {
      return { content: [{ type: 'text', text: 'second echo' }], details: undefined }
    },
  })

  // Not activated on registration, so not offered.
  pi.registerTool({
    name: 'pi_inactive',
    label: 'Inactive',
    description: 'defaultActive: false',
    defaultActive: false,
    parameters: Type.Object({}),
    async execute() {
      return { content: [{ type: 'text', text: 'inactive ran' }], details: undefined }
    },
  })

  // Returns an error instead of throwing.
  pi.registerTool({
    name: 'pi_soft_error',
    label: 'Soft error',
    description: 'Returns isError.',
    parameters: Type.Object({}),
    async execute() {
      return { content: [{ type: 'text', text: 'soft failure' }], details: undefined, isError: true }
    },
  })

  // Overrides pi's built-in `edit`; accepts `edits` as a JSON string, as pi's own edit does.
  pi.registerTool({
    name: 'edit',
    label: 'edit (override)',
    description: 'Edit a file (override).',
    parameters: Type.Object({
      path: Type.String(),
      edits: Type.Array(Type.Object({ oldText: Type.String(), newText: Type.String() })),
    }),
    // In place, as pi's own edit tool does.
    prepareArguments(args: unknown) {
      const a = args as { path: string; edits: unknown }
      if (typeof a.edits === 'string') a.edits = JSON.parse(a.edits)
      return a
    },
    async execute(_id, params) {
      return { content: [{ type: 'text', text: `edit override: ${JSON.stringify(params)}` }], details: undefined }
    },
  })

  // Nested calls through ctx.executeTool: a pi tool, one a policy blocks, and
  // one of yoagent's tools (not reachable). Records each outcome.
  pi.registerTool({
    name: 'pi_compose',
    label: 'Compose',
    description: 'Calls other tools.',
    parameters: Type.Object({}),
    async execute(_id, _params, _signal, _onUpdate, ctx) {
      const callable = ctx.tools.map((t: { name: string }) => t.name)
      const outcomes = []
      for (const [name, args] of [
        ['pi_echo', { text: 'nested SECRET' }],
        ['pi_echo', { text: 'boom' }],
        ['bash', { command: 'echo hi' }],
        // A throw: its error text still goes through the redaction.
        ['pi_fail', { why: 'SECRET leaked' }],
        // No arguments: an empty object, as in pi.
        ['pi_dynamic', undefined],
      ] as const) {
        const o = await ctx.executeTool(name, args)
        const text = o.result.content.map((b: { text?: string }) => b.text ?? '').join('')
        outcomes.push(`${o.toolCall.id} ${name} ${o.isError ? 'error' : 'ok'}: ${text}`)
      }
      return {
        content: [{ type: 'text', text: `callable: ${callable.includes('pi_echo')}\n${outcomes.join('\n')}` }],
        details: undefined,
      }
    },
  })

  // Records and shows its result through pi's session API, as pi video tools do.
  pi.registerTool({
    name: 'pi_render',
    label: 'Render',
    description: 'Renders, then records the job.',
    parameters: Type.Object({}),
    async execute(_id, _params, _signal, _onUpdate, ctx) {
      pi.appendEntry('render:last-job', { path: '/tmp/out.mp4' })
      pi.sendMessage({ customType: 'render_result', content: [{ type: 'text', text: 'Rendered /tmp/out.mp4' }], display: true })
      const entries = ctx.sessionManager.getEntries().filter((e: { type: string }) => e.type === 'custom').length
      return { content: [{ type: 'text', text: `rendered; ${entries} custom entr${entries === 1 ? 'y' : 'ies'} recorded` }], details: undefined }
    },
  })

  pi.on('tool_call', async (event) => {
    if (event.toolName === 'pi_echo' && event.input.text === 'boom') throw new Error('policy crashed')
    // yoagent's search, as pi's grep: include is pi's glob, and an unset
    // case_sensitive is pi's ignoreCase: true (yoagent searches case-insensitively).
    if (event.toolName === 'grep' && event.input.glob === '*.md' && event.input.ignoreCase === true) {
      event.input.glob = '*.txt'
    }
    // yoagent's list_files, as pi's find: the required pattern defaults to '*'.
    if (event.toolName === 'find' && event.input.pattern === '*') event.input.path = `${event.input.path}/inner`
    // A field yoagent's bash does not have: denied, not silently dropped.
    if (event.toolName === 'bash' && event.input.command === 'echo bounded') event.input.timeout = 30
    if (event.toolName === 'bash' && event.input.command === 'echo stop-now') {
      return { block: true, reason: 'stopping the run', terminate: true }
    }
    return undefined
  })

  // Scoped to pi_fail: shows a nested call's thrown error reached tool_result.
  pi.on('tool_result', async (event) =>
    event.toolName === 'pi_fail' && event.isError ? { content: [{ type: 'text', text: 'pi_fail error seen' }] } : undefined,
  )

  // A details-only edit must keep the content (images included).
  pi.on('tool_result', async (event) => (event.toolName === 'read' ? { details: { seen: true } } : undefined))
  // A redaction that fails: the result is withheld.
  pi.on('tool_result', async (event) => {
    const text = event.content.map((block) => (block.type === 'text' ? block.text : '')).join('')
    if (text.includes('BREAK')) throw new Error('redaction crashed')
    return undefined
  })

  // The crashing and the prompt-replacing handlers are skipped; the
  // message-returning one loses only its message; the first fixture's
  // addition and the last handler's still count.
  pi.on('before_agent_start', async () => {
    throw new Error('prompt hook crashed')
  })
  pi.on('before_agent_start', async (event) => ({
    message: { customType: 'x', content: 'injected', display: false },
    systemPrompt: `${event.systemPrompt}\n\nMessage-handler rules: kept.`,
  }))
  pi.on('before_agent_start', async () => ({ systemPrompt: 'a whole new prompt' }))
  // After the failing ones: still counts.
  pi.on('before_agent_start', async (event) => ({ systemPrompt: `${event.systemPrompt}\n\nExtra rules: last.` }))
}
