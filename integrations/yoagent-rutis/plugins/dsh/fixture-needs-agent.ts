// A test fixture: dsh tools that need the calling DSH agent, as many dsh
// plugins do (`dsh-host.ts` supplies a stand-in). `tests/dsh_test.rs` loads
// it. Not for production use.
//
//   fixture_where  {}      → "<agent id> <session cwd>", or fails without an
//                            agent (as dsh-univer-office's tools do)
//   fixture_steer  {text}  → steers the calling agent with the text
//                            (`agent.steer`), → "steered"
//   fixture_turns  {}      → the agent's session's turns, as JSON:
//                            {starts, ends: ["completed", "aborted:user", …]}

import { defineTool } from '@deepseek-ai/dsh-tools'

export const name = 'yoagent-dsh-fixture-agent'
export const inject = ['tools']

const TEXT_OUTPUT = {
  schema: { type: 'string' } as const,
  render: (_args: unknown, value: string) => [{ type: 'text' as const, text: value }],
}

type Event = { type: string; data?: { reason?: { kind: string; reason?: { kind: string } } } }
type Exec = {
  agent?: {
    id: string
    session: { header: { cwd?: string }; seq: number | bigint; eventAt(i: number): Event }
    steer(m: unknown): void
  }
}

const agentOf = (exec: Exec | undefined) => {
  if (!exec?.agent) throw new Error('SESSION_SCOPE_UNAVAILABLE: this tool needs a calling agent with a workspace')
  return exec.agent
}

// The Cordis context is used untyped: Cordis types are not needed at runtime.
export function apply(ctx: any) {
  const tools = [
    defineTool({
      name: 'fixture_where',
      description: 'Where the calling agent works.',
      parameters: {},
      output: TEXT_OUTPUT,
      async execute(_args: unknown, exec: Exec) {
        const agent = agentOf(exec)
        return `${agent.id} ${agent.session.header.cwd}`
      },
    }),
    defineTool({
      name: 'fixture_steer',
      description: 'Steer the calling agent.',
      parameters: { text: { type: 'string', required: true } },
      output: TEXT_OUTPUT,
      async execute(args: { text: string }, exec: Exec) {
        agentOf(exec).steer({ id: 'fixture-steer', role: 'user', content: [{ type: 'text', text: args.text }], source: { kind: 'user' } })
        return 'steered'
      },
    }),
    defineTool({
      name: 'fixture_turns',
      description: "How many turns the calling agent's session opened.",
      parameters: {},
      output: TEXT_OUTPUT,
      async execute(_args: unknown, exec: Exec) {
        const session = agentOf(exec).session
        let starts = 0
        const ends: string[] = []
        for (let i = 0; i < Number(session.seq); i++) {
          const event = session.eventAt(i)
          if (event?.type === 'turn/start') starts++
          if (event?.type === 'turn/end') {
            const reason = event.data?.reason
            ends.push(reason?.reason ? `${reason.kind}:${reason.reason.kind}` : String(reason?.kind))
          }
        }
        return JSON.stringify({ starts, ends })
      },
    }),
  ]
  for (const tool of tools) ctx.effect(() => ctx.tools.register(tool))
}
