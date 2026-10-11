// A pi extension in the style of pi's own permission gate: approval
// guardrails for selected operations that delete, discard or overwrite what
// the owner already has. In pi's print mode (no UI) it blocks them; with a
// frontend attached through yoagent-frontend, the question reaches the
// terminal or browser.
//
// What asks (each shell command is parsed: the program's base name, after
// `env`, `command`, `nohup`, `time` and variable assignments; git's
// subcommand after its global options):
// - `rm`, `unlink`, `rmdir` (globs and expansions always);
// - git: `reset --hard`, `clean`, `checkout` / `restore` of paths (not
//   `restore --staged`), `branch -D`, a forced push, `stash drop` / `clear`;
// - `mv`, `cp`, `tee` (without `-a`) and any `>` redirection (`>`, `1>`,
//   `2>`, `&>`; not `>>`) onto a file that exists;
// - `sudo`, `find … -delete`, `truncate`, `shred`, `mkfs`, `dd`, recursive
//   `chmod` / `chown`;
// - a whole-file write over a file that exists, and any change to `.env`
//   or `.git/`.
// What does not: files the agent created in this session (its deliverable,
// kept up to date), anything in its scratch folder when the host names one
// in `YOAGENT_SCRATCH`, and targeted edits (`edit`) of existing files.
// Paths are compared with symlinks resolved (on macOS /tmp is /private/tmp).
//
// Not a sandbox: a script or program can still delete or overwrite files
// in ways a command line does not show (`python3 -c …`, a Makefile). For a
// guarantee, run the agent in a container or a disposable copy.

import { existsSync, realpathSync } from 'node:fs'
import { basename, dirname, isAbsolute, resolve, sep } from 'node:path'

const ALWAYS = [
  /\bsudo\b/,
  /\bfind\b[^;&|]*\s-delete\b/,
  /\b(truncate|shred|mkfs(\.\w+)?)\b/,
  /\bdd\s+if=/,
  /\bch(mod|own)\s+(-\w*\s+)*-R\b/,
]
const PROTECTED = /(^|\/)(\.env|\.git\/)/
/** Where a redirection is harmless. */
const DEVICES = new Set(['/dev/null', '/dev/stdout', '/dev/stderr', '/dev/tty'])

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

/** Each simple command's words (split on ; && || | and newlines), quotes dropped. */
const segments = (command: string) =>
  command
    .split(/;|&&|\|\||\||\n/)
    .map((part) => part.trim().split(/\s+/).filter(Boolean).map((w) => w.replace(/^["']|["']$/g, '')))
    .filter((words) => words.length > 0)

/** The program (base name) and its arguments, past wrappers and `VAR=value` assignments. */
function program(words: string[]): [string, string[]] {
  let i = 0
  while (i < words.length && (/^\w+=/.test(words[i]) || ['env', 'command', 'nohup', 'time', 'exec'].includes(words[i]))) i++
  return [basename(words[i] ?? ''), words.slice(i + 1)]
}

/** git's subcommand and its arguments, past global options (`-C dir`, `-c k=v`, `--git-dir=…`). */
function gitCommand(args: string[]): [string, string[]] {
  let i = 0
  while (i < args.length && args[i].startsWith('-')) {
    if (['-C', '-c', '--git-dir', '--work-tree', '--namespace'].includes(args[i])) i++
    i++
  }
  return [args[i] ?? '', args.slice(i + 1)]
}

export default function (pi: any) {
  // Files the agent made this session: it may overwrite or delete them.
  const created = new Set<string>()
  const where = (cwd: string, file: string) => real(isAbsolute(file) ? resolve(file) : resolve(cwd, file))
  const free = (path: string) =>
    created.has(path) || (SCRATCH !== undefined && (path === SCRATCH || path.startsWith(SCRATCH + sep)))
  // A path the agent may not touch without asking: one that exists and is not its own.
  const owners = (path: string) => !free(path) && existsSync(path)

  function gitRisky(args: string[], cwd: string): boolean {
    const [sub, rest] = gitCommand(args)
    const flags = new Set(rest.filter((a) => a.startsWith('-')))
    const paths = rest.filter((a) => !a.startsWith('-'))
    switch (sub) {
      case 'reset':
        return flags.has('--hard') || flags.has('--merge') || flags.has('--keep')
      case 'clean':
        return true
      case 'restore':
        // Unstaging only touches the index; anything else rewrites files.
        return !(flags.has('--staged') && !flags.has('--worktree') && !flags.has('-W'))
      case 'checkout':
        // A branch switch is fine; checking out paths discards their changes.
        return rest.includes('--') || paths.includes('.') || paths.some((p) => owners(where(cwd, p)))
      case 'branch':
        return flags.has('-D') || (flags.has('-d') && flags.has('--force'))
      case 'push':
        return [...flags].some((f) => f === '-f' || f.startsWith('--force') || f.startsWith('--mirror') || f === '--delete')
      case 'stash':
        return paths[0] === 'drop' || paths[0] === 'clear'
      default:
        return false
    }
  }

  /** Whether this command needs the owner's yes. */
  function risky(command: string, cwd: string): boolean {
    if (ALWAYS.some((pattern) => pattern.test(command))) return true
    for (const words of segments(command)) {
      const [name, args] = program(words)
      const operands = args.filter((word) => !word.startsWith('-') && !/^\d*>|^&>/.test(word))
      if (name === 'rm' || name === 'unlink' || name === 'rmdir') {
        // Globs and expansions cannot be checked: ask.
        if (operands.length === 0 || operands.some((a) => /[*?$`{]/.test(a))) return true
        if (operands.some((a) => !free(where(cwd, a)))) return true
      }
      if (name === 'git' && gitRisky(args, cwd)) return true
      if ((name === 'mv' || name === 'cp') && operands.length > 0 && owners(where(cwd, operands.at(-1)!))) return true
      if (name === 'tee' && !args.includes('-a') && !args.includes('--append')) {
        if (operands.some((a) => owners(where(cwd, a)))) return true
      }
    }
    // Every redirection that truncates (`>`, `1>`, `2>`, `&>`; not `>>` or `>&`).
    for (const match of command.matchAll(/(?<!>)>(?![>&])\s*([^\s;&|<>]+)/g)) {
      const target = match[1].replace(/^["']|["']$/g, '')
      if (DEVICES.has(target)) continue
      const path = where(cwd, target)
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
