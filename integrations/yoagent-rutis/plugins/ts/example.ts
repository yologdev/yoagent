// A TypeScript plugin for yoagent-rutis: a tool, a deny policy and an
// output redactor, registered as one handler.
//
// Load it as a rutis-loader row named after this file (the Node runtime runs
// TypeScript directly), with `yoagent` shared in the loader's catalog. The
// end-to-end tests do (`tests/languages_test.rs`).

import { definePlugin } from '@arcships/rutis'
import type { Yoagent, ToolCall, ToolOutput } from '../yoagent.d.ts'

export interface Config {
  /** Tools this plugin refuses to let run. */
  denied?: string[]
}

export default definePlugin<Config>({
  inject: ['yoagent'],
  config: {
    type: 'object',
    properties: { denied: { type: 'array', items: { type: 'string' } } },
  },
  apply(ctx, config) {
    const denied = new Set(config.denied ?? ['bash'])
    const yoagent = ctx.use<Yoagent>('yoagent')
    ctx.effect(
      yoagent.register('ts-example', {
        async tools() {
          return [
            {
              name: 'ts_word_count',
              description: 'Count the words in a text',
              parameters: {
                type: 'object',
                properties: { text: { type: 'string' } },
                required: ['text'],
              },
            },
          ]
        },

        async call_tool(call: ToolCall) {
          const text = String(call.args.text ?? '')
          const words = text.split(/\s+/).filter(Boolean).length
          return { text: `${words} words`, details: { words } }
        },

        async before_tool(call: ToolCall) {
          if (denied.has(call.tool)) return { deny: `\`${call.tool}\` is disabled by the ts-example plugin` }
        },

        // Mask anything that looks like an API key, in every tool's output.
        async after_tool(_call: ToolCall, output: ToolOutput) {
          if (!/\bsk-[\w-]+/.test(output.text)) return
          return { text: output.text.replace(/\bsk-[\w-]+/g, '[key]') }
        },
      }),
    )
  },
})
