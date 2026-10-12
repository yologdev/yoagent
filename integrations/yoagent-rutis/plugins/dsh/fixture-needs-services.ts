// A test fixture: a dsh tool plugin that needs DSH's host services, as many
// dsh plugins do (`plugins/dsh/host-services.json`). It starts only once all
// of them exist; then it writes `config.ready` and registers one tool:
//
//   fixture_services {}  → "credentials: <value of YOAGENT_TEST_KEY>"
//                          read through the `credentials` service
//
// `tests/dsh_test.rs` loads it with and without the host services. Not for
// production use.

import { writeFileSync } from 'node:fs'
import { defineTool } from '@deepseek-ai/dsh-tools'

export const name = 'yoagent-dsh-fixture-services'
export const inject = ['tools', 'fs', 'attachments', 'credentials', 'storage', 'sessionPersistence', 'skills']

export interface Config {
  /** Written once the plugin starts (all its services exist). */
  ready?: string
}

// The Cordis context is used untyped: Cordis types are not needed at runtime.
export function apply(ctx: any, config: Config | undefined) {
  const tool = defineTool({
    name: 'fixture_services',
    description: 'Report a credential read through the host services.',
    parameters: {},
    output: {
      schema: { type: 'string' } as const,
      render: (_args: unknown, value: string) => [{ type: 'text' as const, text: value }],
    },
    async execute() {
      const found = await ctx.credentials.resolve('YOAGENT_TEST_KEY')
      return `credentials: ${found?.value ?? 'none'}`
    },
  })
  ctx.effect(() => ctx.tools.register(tool), 'fixture: fixture_services')
  if (config?.ready) writeFileSync(config.ready, 'started')
}
