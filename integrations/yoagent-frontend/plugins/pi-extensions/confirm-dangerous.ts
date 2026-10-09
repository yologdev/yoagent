// A pi extension in the style of pi's own permission gate: a dangerous bash
// command or a write to a protected path asks the user first. In pi's print
// mode (no UI) it blocks them; with a frontend attached through
// yoagent-frontend, the question reaches the terminal or browser.

const DANGEROUS = /\b(rm\s+-[a-z]*r|sudo|chmod\s+-R|git\s+push\s+--force|mkfs|dd\s+if=)/
const PROTECTED = /(^|\/)(\.env|\.git\/)/

export default function (pi: any) {
  pi.on('tool_call', async (event: any, ctx: any) => {
    const subject =
      event.toolName === 'bash'
        ? DANGEROUS.test(String(event.input?.command ?? '')) && `Run \`${event.input.command}\`?`
        : ['write', 'edit'].includes(event.toolName) && PROTECTED.test(String(event.input?.path ?? ''))
          ? `Change the protected file ${event.input.path}?`
          : false
    if (!subject) return undefined
    if (!ctx.hasUI) return { block: true, reason: 'needs confirmation, and no one is there to ask' }
    const choice = await ctx.ui.select(subject, ['Allow', 'Block'])
    return choice === 'Allow' ? undefined : { block: true, reason: 'the user blocked it' }
  })
}
