// A second pi extension for `tests/pi_test.rs`, loaded after
// `fixture-extension.ts`: the less common paths.

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
    prepareArguments(args: unknown) {
      const a = args as { path: string; edits: unknown }
      return typeof a.edits === 'string' ? { ...a, edits: JSON.parse(a.edits) } : a
    },
    async execute(_id, params) {
      return { content: [{ type: 'text', text: `edit override: ${JSON.stringify(params)}` }], details: undefined }
    },
  })

  pi.on('tool_call', async (event) => {
    if (event.toolName === 'pi_echo' && event.input.text === 'boom') throw new Error('policy crashed')
    // yoagent's search, as pi's grep: its include is pi's glob.
    if (event.toolName === 'grep' && event.input.glob === '*.md') event.input.glob = '*.txt'
    return undefined
  })

  // A details-only edit must keep the content (images included).
  pi.on('tool_result', async (event) => (event.toolName === 'read' ? { details: { seen: true } } : undefined))

  // Each is skipped on its own; the first fixture's addition still counts.
  pi.on('before_agent_start', async () => {
    throw new Error('prompt hook crashed')
  })
  pi.on('before_agent_start', async () => ({
    message: { customType: 'x', content: 'injected', display: false },
  }))
  pi.on('before_agent_start', async (event) => ({ systemPrompt: 'a whole new prompt' }))
}
