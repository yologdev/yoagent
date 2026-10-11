// A pi extension in the style of pi's own permission gate: anything that
// could destroy or overwrite what the owner already has asks first. In pi's
// print mode (no UI) it blocks; with a frontend attached through
// yoagent-frontend, the question reaches the terminal or browser.
//
// What asks:
// - shell commands that delete, discard or overwrite: `rm`, `git reset
//   --hard`, `git clean`, `git checkout -- …` / `git restore`, `git branch
//   -D`, a forced push, `find … -delete`, `truncate`, `shred`, `mkfs`, `dd`,
//   `sudo`, recursive `chmod` / `chown`, and `mv` or a `>` redirection onto
//   a file that exists;
// - a write over a file that exists, and any change to `.env` or `.git/`.
// What does not: files the agent created in this session (its deliverable,
// kept up to date) and anything in its scratch folder, when the host names
// one in `YOAGENT_SCRATCH` (the handoff agent does). Paths are compared
// with symlinks resolved (on macOS /tmp is /private/tmp).

import { existsSync, realpathSync } from 'node:fs'
import { basename, dirname, isAbsolute, resolve, sep } from 'node:path'

const ALWAYS = [
  /\bsudo\b/,
  /\bgit\s+reset\s+--hard\b/,
  /\bgit\s+clean\b/,
  /\bgit\s+(checkout|restore)\b[^;&|]*(\s--\s|\s\.(\s|$))/,
  /\bgit\s+branch\s+-D\b/,
  /\bgit\s+push\b[^;&|]*\s(--force\b|-f\b)/,
  /\bfind\b[^;&|]*\s-delete\b/,
  /\b(truncate|shred|mkfs(\.\w+)?)\b/,
  /\bdd\s+if=/,
  /\bch(mod|own)\s+-R\b/,
]
const PROTECTED = /(^|\/)(\.env|\.git\/)/

/** `path` with symlinks resolved, also for a file not created yet. */
function real(path: string): string {
  try {
    return realpathSync(path)
  } catch {
    const parent = dirname(path)
    return parent === path ? path : resolve(real(parent), basename(path))
  }
}

const SCRATCH = process.env.YOAGENT_SCRATCH ? real(resolve(process.env.YOAGENT_SCRATCH)) : undefined

/** The words of each simple command in `command` (split on ; && || |). */
const segments = (command: string) =>
  command
    .split(/;|&&|\|\||\|/)
    .map((part) => part.trim().split(/\s+/).filter(Boolean))
    .filter((words) => words.length > 0)

export default function (pi: any) {
  // Files the agent made this session: it may overwrite or delete them.
  const created = new Set<string>()
  const where = (cwd: string, file: string) => real(isAbsolute(file) ? resolve(file) : resolve(cwd, file))
  const free = (path: string) =>
    created.has(path) || (SCRATCH !== undefined && (path === SCRATCH || path.startsWith(SCRATCH + sep)))
  // A path the agent may not touch without asking: one that exists and is not its own.
  const owners = (path: string) => !free(path) && existsSync(path)

  /** Why this command needs the owner's yes, or nothing. */
  const risky = (command: string, cwd: string): boolean => {
    if (ALWAYS.some((pattern) => pattern.test(command))) return true
    for (const words of segments(command)) {
      const program = words[0]
      const args = words.slice(1).filter((word) => !word.startsWith('-'))
      if (program === 'rm' || program === 'unlink' || program === 'rmdir') {
        // Globs and expansions cannot be checked: ask.
        if (args.length === 0 || args.some((a) => /[*?$`{]/.test(a))) return true
        if (args.some((a) => !free(where(cwd, a)))) return true
      }
      if (program === 'mv' || program === 'cp') {
        const target = args.at(-1)
        if (target && owners(where(cwd, target))) return true
      }
    }
    // `>` (not `>>`, `2>`, `>&`) onto a file that exists and is not the agent's.
    for (const match of command.matchAll(/(?<![>&\d])>(?![>&])\s*([^\s;&|<>]+)/g)) {
      const path = where(cwd, match[1].replace(/^["']|["']$/g, ''))
      if (owners(path)) return true
      if (!existsSync(path)) created.add(path)
    }
    return false
  }

  pi.on('tool_call', async (event: any, ctx: any) => {
    const cwd = ctx.cwd ?? process.cwd()
    let subject: string | false = false
    let creating: string | undefined
    if (event.toolName === 'bash') {
      const command = String(event.input?.command ?? '')
      if (risky(command, cwd)) subject = `Run \`${command}\`?`
    } else if (['write', 'edit'].includes(event.toolName)) {
      const file = String(event.input?.path ?? '')
      const path = where(cwd, file)
      if (PROTECTED.test(file)) subject = `Change the protected file ${file}?`
      else if (event.toolName === 'write' && owners(path)) subject = `Overwrite the existing file ${file}?`
      else if (event.toolName === 'write' && !existsSync(path)) creating = path
    }
    if (subject) {
      if (!ctx.hasUI) return { block: true, reason: 'needs confirmation, and no one is there to ask' }
      const choice = await ctx.ui.select(subject, ['Allow', 'Block'])
      if (choice !== 'Allow') return { block: true, reason: 'the user blocked it' }
    }
    if (creating) created.add(creating)
    return undefined
  })
}
