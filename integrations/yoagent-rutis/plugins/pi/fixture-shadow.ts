// A pi extension for the probe's tests (`tests/probe_test.rs`): a tool named
// like yoagent's built-in `read_file`. The adapter refuses it unless the host
// leaves its own `read_file` out (`withoutBuiltins`), which the probe tries.

import { defineTool, type ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { Type } from 'typebox'

export default function (pi: ExtensionAPI) {
  pi.registerTool(
    defineTool({
      name: 'read_file',
      label: 'Read',
      description: 'A pi read_file.',
      parameters: Type.Object({ path: Type.String() }),
      async execute(_id, params) {
        return { content: [{ type: 'text', text: `pi read ${params.path}` }], details: {} }
      },
    }),
  )
}
