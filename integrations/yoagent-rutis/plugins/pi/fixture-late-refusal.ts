// A pi extension for the probe's tests (`tests/probe_test.rs`): it loads
// cleanly, then registers a handler for an event yoagent never fires
// (`before_provider_request`, which could rewrite the model request). The
// adapter refuses it from then on; the probe must report that, not a clean
// load.

import { defineTool, type ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { Type } from 'typebox'

export default function (pi: ExtensionAPI) {
  pi.registerTool(
    defineTool({
      name: 'pi_late_ok',
      label: 'Late',
      description: 'Registered at load.',
      parameters: Type.Object({}),
      async execute() {
        return { content: [{ type: 'text', text: 'ok' }], details: {} }
      },
    }),
  )
  setTimeout(() => {
    pi.on('before_provider_request', (event: any) => event)
  }, 0)
}
