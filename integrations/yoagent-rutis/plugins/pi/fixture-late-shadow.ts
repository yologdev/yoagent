// A pi extension for the probe's tests (`tests/probe_test.rs`): after load,
// it registers a tool named like yoagent's built-in `read_file`. The adapter
// refuses it then; the probe retries with that built-in left out
// (`withoutBuiltins`), as it does for one registered at load.

import { defineTool, type ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { Type } from 'typebox'

export default function (pi: ExtensionAPI) {
  setTimeout(() => {
    pi.registerTool(
      defineTool({
        name: 'read_file',
        label: 'Read',
        description: 'A pi read_file, registered late.',
        parameters: Type.Object({ path: Type.String() }),
        async execute(_id, params) {
          return { content: [{ type: 'text', text: `pi read ${params.path}` }], details: {} }
        },
      }),
    )
  }, 0)
}
