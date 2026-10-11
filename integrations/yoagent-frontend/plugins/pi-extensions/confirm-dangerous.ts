// A pi extension in the style of pi's own permission gate: approval
// guardrails for selected operations that delete, discard or overwrite what
// the owner already has. In pi's print mode (no UI) it blocks them; with a
// frontend attached through yoagent-frontend, the question reaches the
// terminal or browser.
//
// Each shell command is tokenized (quotes, escapes, operators, redirections;
// the bodies of unquoted `<<WORD` heredocs skipped) and read command by
// command, keeping track of the directory `cd` moves to: only where the cd
// surely applies (a literal directory that exists; `( … )` restores the
// directory after; a cd in a pipeline, in the background or before `||`, or
// to a missing directory, makes it unknown). What asks:
// - `rm`, `unlink`, `rmdir`, `git rm` (not `--cached`), `xargs` running one
//   of them, `find … -delete` or `-exec rm …`;
// - git: `reset --hard`, `clean` (not `-n`), `checkout` / `restore` of
//   paths (not `restore --staged`), `checkout -f`, `switch -f` /
//   `--discard-changes`, `branch -D`, a forced push (`-f`, `--force*`,
//   `+ref`, `:ref`, `--mirror`, `--delete`), `stash drop` / `clear`;
// - `mv`, `cp` (also into a directory), `tee` (without `-a`), `dd of=` and
//   every truncating redirection (`>`, `>|`, `1>`, `2>`, `&>`; not `>>` or
//   to `/dev/null`) onto a file that exists;
// - `sudo`, `truncate`, `shred`, `mkfs`, recursive `chmod` / `chown`;
// - `bash -c` / `sh -c` scripts and `eval` of literal text, read the same
//   way, and what command substitutions run (`$( … )`, backquotes, `<( … )`,
//   `>( … )`; also inside double quotes and unquoted heredoc bodies, which
//   the shell expands), each where its command runs; `eval` of unknown text;
// - a whole-file write over a file that exists, and any change to `.env`
//   or `.git/`;
// - any path it cannot know: `$VAR`, `~`, backquotes, globs, or after a
//   `cd` it cannot follow.
// What does not: files the agent created in this session (by a write, a
// redirection, `tee`, `cp` or `mv` it was allowed to run), anything in its
// scratch folder when the host names one in `YOAGENT_SCRATCH`, and targeted
// edits (`edit`) of existing files. Paths are compared with symlinks
// resolved (on macOS /tmp is /private/tmp).
//
// Not a sandbox: a script or program can still change files in ways a
// command line does not show (`sed -i`, `python3 -c …`, a Makefile). For a
// guarantee, run the agent in a container or a disposable copy.

import { existsSync, realpathSync, statSync } from 'node:fs'
import { basename, dirname, isAbsolute, resolve, sep } from 'node:path'

const PROTECTED = /(^|\/)(\.env|\.git\/)/
/** Where a redirection is harmless. */
const DEVICES = new Set(['/dev/null', '/dev/stdout', '/dev/stderr', '/dev/tty'])
const ALWAYS = new Set(['sudo', 'doas', 'truncate', 'shred', 'mkfs', 'wipefs'])
const DELETERS = new Set(['rm', 'unlink', 'rmdir', 'shred'])
const SHELLS = new Set(['sh', 'bash', 'zsh', 'dash', 'ksh'])

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

/** A word as the shell would see it; `known` is false when it holds an expansion. */
type Word = { text: string; known: boolean }
/**
 * One simple command: its words, the redirections that truncate a file, the
 * commands its substitutions run (`$( … )`, backticks, `<( … )`, `>( … )`,
 * also inside double quotes and unquoted heredoc bodies), and the operator
 * after it (`;`, `\n`, `&&`, `||`, `|`, `&`, or `end`).
 */
type Simple = { words: Word[]; truncates: Word[]; runs: string[]; after: string }
/** A command, or the start / end of a `( … )` subshell. */
type Item = Simple | 'open' | 'close'

/** The index of the backquote closing the one at `open`; -1 if none. */
function endOfBackquote(s: string, open: number): number {
  for (let i = open + 1; i < s.length; i++) {
    if (s[i] === '\\') i++
    else if (s[i] === '`') return i
  }
  return -1
}

/** The index of the `)` closing the `(` at `open`, past quotes and nesting; -1 if none. */
function closing(s: string, open: number): number {
  let depth = 0
  for (let i = open; i < s.length; i++) {
    const c = s[i]
    if (c === '\\') i++
    else if (c === "'") {
      i = s.indexOf("'", i + 1)
      if (i < 0) return -1
    } else if (c === '"') {
      i = endOfDouble(s, i)
      if (i < 0) return -1
    } else if (c === '`') {
      i = endOfBackquote(s, i)
      if (i < 0) return -1
    } else if (c === '(') depth++
    else if (c === ')' && --depth === 0) return i
  }
  return -1
}

/** The index of the `"` closing the one at `open`, past substitutions inside; -1 if none. */
function endOfDouble(s: string, open: number): number {
  for (let i = open + 1; i < s.length; i++) {
    const c = s[i]
    if (c === '\\') i++
    else if (c === '"') return i
    else if (c === '`') {
      i = endOfBackquote(s, i)
      if (i < 0) return -1
    } else if (c === '$' && s[i + 1] === '(') {
      i = closing(s, i + 1)
      if (i < 0) return -1
    }
  }
  return -1
}

/**
 * The commands that expanding `text` runs (as in double quotes or an
 * unquoted heredoc body): every `$( … )` and backquoted command, and those
 * inside `$(( … ))`. `null` when one is not closed.
 */
function substitutions(text: string): string[] | null {
  const runs: string[] = []
  for (let i = 0; i < text.length; i++) {
    const c = text[i]
    if (c === '\\') i++
    else if (c === '`') {
      const end = endOfBackquote(text, i)
      if (end < 0) return null
      runs.push(text.slice(i + 1, end).replace(/\\`/g, '`'))
      i = end
    } else if (c === '$' && text[i + 1] === '(') {
      const end = closing(text, i + 1)
      if (end < 0) return null
      const inner = text.slice(i + 2, end)
      if (text[i + 2] === '(') {
        // Arithmetic `$(( … ))`: only what it substitutes runs.
        const nested = substitutions(inner.slice(1, -1))
        if (!nested) return null
        runs.push(...nested)
      } else runs.push(inner)
      i = end
    }
  }
  return runs
}

/**
 * Split a command line into simple commands and subshell markers. Quotes,
 * escapes, substitutions and redirections are read as the shell reads
 * them; a heredoc (an unquoted `<<WORD`) has its body skipped, from the
 * next line to the delimiter — unless the delimiter is unquoted, when the
 * shell expands the body and the commands it substitutes run. `null` when
 * the line cannot be read (an unclosed quote or substitution).
 */
function parse(s: string): Item[] | null {
  const out: Item[] = []
  let cur: Simple = { words: [], truncates: [], runs: [], after: 'end' }
  let word: Word | null = null
  let redirect: 'truncate' | 'skip' | null = null
  // Heredocs waiting for the end of the current line, with the command they feed.
  const heredocs: { delimiter: string; strip: boolean; expands: boolean; feeds: Simple }[] = []
  const finishWord = () => {
    if (!word) return
    if (redirect === 'truncate') cur.truncates.push(word)
    else if (redirect !== 'skip') cur.words.push(word)
    redirect = null
    word = null
  }
  const finishCommand = (after: string) => {
    finishWord()
    if (cur.words.length > 0 || cur.truncates.length > 0 || cur.runs.length > 0) {
      cur.after = after
      out.push(cur)
    }
    cur = { words: [], truncates: [], runs: [], after: 'end' }
  }
  const unknown = () => {
    word ??= { text: '', known: true }
    word.known = false
  }
  for (let i = 0; i < s.length; i++) {
    const c = s[i]
    if (c === "'") {
      const end = s.indexOf("'", i + 1)
      if (end < 0) return null
      word ??= { text: '', known: true }
      word.text += s.slice(i + 1, end)
      i = end
      continue
    }
    if (c === '"') {
      const end = endOfDouble(s, i)
      if (end < 0) return null
      const body = s.slice(i + 1, end)
      const runs = substitutions(body)
      if (!runs) return null
      cur.runs.push(...runs)
      word ??= { text: '', known: true }
      word.text += body
      if (/[$`]/.test(body)) word.known = false
      i = end
      continue
    }
    if (c === '\\') {
      word ??= { text: '', known: true }
      word.text += s[i + 1] ?? ''
      i++
      continue
    }
    if (c === '`') {
      const end = endOfBackquote(s, i)
      if (end < 0) return null
      cur.runs.push(s.slice(i + 1, end).replace(/\\`/g, '`'))
      unknown()
      i = end
      continue
    }
    if (c === '$' && s[i + 1] === '(') {
      const end = closing(s, i + 1)
      if (end < 0) return null
      const runs = substitutions(s.slice(i, end + 1))
      if (!runs) return null
      cur.runs.push(...runs)
      unknown()
      i = end
      continue
    }
    if ((c === '<' || c === '>') && s[i + 1] === '(') {
      // Process substitution: the command runs; the word is a path to its pipe.
      const end = closing(s, i + 1)
      if (end < 0) return null
      cur.runs.push(s.slice(i + 2, end))
      unknown()
      i = end
      continue
    }
    if (c === '\n') {
      finishCommand('\n')
      for (const { delimiter, strip, expands, feeds } of heredocs.splice(0)) {
        let next = s.indexOf('\n', i + 1)
        const body: string[] = []
        for (;;) {
          const lineEnd = next < 0 ? s.length : next
          const line = s.slice(i + 1, lineEnd)
          i = lineEnd
          if ((strip ? line.replace(/^\t+/, '') : line) === delimiter) break
          body.push(line)
          if (next < 0) break
          next = s.indexOf('\n', i + 1)
        }
        if (expands) {
          // An unquoted delimiter: the shell expands the body, running what it substitutes.
          const runs = substitutions(body.join('\n'))
          if (!runs) return null
          feeds.runs.push(...runs)
        }
      }
      continue
    }
    if (/\s/.test(c)) {
      finishWord()
      continue
    }
    if (c === ';') {
      finishCommand(';')
      continue
    }
    if (c === '(' || c === ')') {
      finishCommand(c === '(' ? ';' : 'end')
      out.push(c === '(' ? 'open' : 'close')
      continue
    }
    if (c === '|' || c === '&') {
      if (c === '&' && s[i + 1] === '>') {
        // `&>file`, `&>>file`
        finishWord()
        redirect = s[i + 2] === '>' ? 'skip' : 'truncate'
        i += s[i + 2] === '>' ? 2 : 1
        continue
      }
      const double = s[i + 1] === c
      if (double) i++
      finishCommand(double ? c + c : c)
      continue
    }
    if (c === '<') {
      finishWord()
      if (s[i + 1] === '<' && s[i + 2] === '<') {
        // A here-string: its word is data (its substitutions still run).
        redirect = 'skip'
        i += 2
        continue
      }
      if (s[i + 1] === '<') {
        // A heredoc: read its delimiter; the body comes after this line.
        i++
        const strip = s[i + 1] === '-'
        if (strip) i++
        while (s[i + 1] === ' ' || s[i + 1] === '\t') i++
        const m = s.slice(i + 1).match(/^(?:'([^']*)'|"([^"]*)"|([^\s;&|<>()]+))/)
        if (!m) return null
        const quoted = m[1] !== undefined || m[2] !== undefined || m[3].includes('\\')
        heredocs.push({
          delimiter: m[1] ?? m[2] ?? m[3].replace(/\\/g, ''),
          strip,
          expands: !quoted,
          feeds: cur,
        })
        i += m[0].length
        continue
      }
      redirect = 'skip'
      continue
    }
    if (c === '>') {
      // A file descriptor number right before it belongs to the operator.
      if (word && /^\d+$/.test(word.text) && word.known) word = null
      else finishWord()
      if (s[i + 1] === '>') {
        redirect = 'skip'
        i++
      } else if (s[i + 1] === '&') {
        redirect = 'skip'
        i++
      } else {
        redirect = 'truncate'
        if (s[i + 1] === '|') i++
      }
      continue
    }
    word ??= { text: '', known: true }
    if (c === '$' || c === '*' || c === '?' || c === '{' || c === '[') word.known = false
    if (c === '~' && word.text === '') word.known = false
    word.text += c
  }
  finishCommand('end')
  return out
}

/** Past wrappers that run another program: their options, and the program. */
function unwrap(words: Word[]): { name: string; args: Word[]; viaXargs: boolean } {
  let i = 0
  let viaXargs = false
  for (;;) {
    while (i < words.length && /^\w+=/.test(words[i].text)) i++
    const w = words[i]?.text.replace(/^\\/, '') ?? ''
    const name = basename(w)
    if (['command', 'nohup', 'exec', 'builtin'].includes(name)) i++
    else if (name === 'time') {
      i++
      while (words[i]?.text.startsWith('-')) i++
    } else if (name === 'env' || name === 'stdbuf' || name === 'nice' || name === 'ionice') {
      i++
      while (words[i] && (words[i].text.startsWith('-') || /^\w+=/.test(words[i].text))) {
        if (['-u', '-n', '-c', '-C', '-S'].includes(words[i].text)) i++
        i++
      }
    } else if (name === 'timeout') {
      i++
      while (words[i]?.text.startsWith('-')) {
        if (['-s', '-k', '--signal', '--kill-after'].includes(words[i].text)) i++
        i++
      }
      i++ // the duration
    } else if (name === 'xargs') {
      viaXargs = true
      i++
      while (words[i]?.text.startsWith('-')) {
        if (/^-[IdEsnLPa]$/.test(words[i].text)) i++
        i++
      }
    } else {
      return { name, args: words.slice(i + 1), viaXargs }
    }
  }
}

export default function (pi: any) {
  // Files the agent made this session: it may overwrite or delete them.
  const created = new Set<string>()
  const where = (cwd: string, file: string) => real(isAbsolute(file) ? resolve(file) : resolve(cwd, file))
  const free = (path: string) =>
    created.has(path) || (SCRATCH !== undefined && (path === SCRATCH || path.startsWith(SCRATCH + sep)))
  // A path the agent may not touch without asking: one that exists and is not its own.
  const owners = (path: string) => !free(path) && existsSync(path)
  const isDir = (path: string) => {
    try {
      return statSync(path).isDirectory()
    } catch {
      return false
    }
  }

  /**
   * Whether `line` needs the owner's yes, and the files it creates (added to
   * `created` only if it runs). `cwd` is `null` once a `cd` cannot be
   * followed: every path is then unknown.
   */
  function check(line: string, start: string): { ask: boolean; creates: string[] } {
    const creates: string[] = []
    const ask = { ask: true, creates }
    const commands = parse(line)
    if (!commands) return ask
    let cwd: string | null = start
    // A target this command would write: asks if it is the owner's, or unknown.
    const target = (w: Word): boolean => {
      if (DEVICES.has(w.text)) return false
      if (!w.known || cwd === null) return true
      const path = where(cwd, w.text)
      if (owners(path)) return true
      if (!existsSync(path)) creates.push(path)
      return false
    }
    // A path this command would delete: asks unless it is the agent's own.
    const deletes = (w: Word): boolean => !w.known || cwd === null || !free(where(cwd, w.text))

    // Directories to return to when a `( … )` subshell ends.
    const saved: (string | null)[] = []
    let before = ';'
    for (const item of commands) {
      if (item === 'open') {
        saved.push(cwd)
        continue
      }
      if (item === 'close') {
        if (saved.length === 0) return ask
        cwd = saved.pop()!
        continue
      }
      const { words, truncates, runs, after } = item
      const piped = before === '|' || after === '|'
      before = after
      // What its substitutions run, where it runs (a subshell of this directory).
      for (const inner of runs) {
        if (cwd === null) return ask
        const verdict = check(inner, cwd)
        if (verdict.ask) return ask
        creates.push(...verdict.creates)
      }
      if (truncates.some(target)) return ask
      if (words.length === 0) continue
      const { name, args, viaXargs } = unwrap(words)
      const flags = args.filter((a) => a.text.startsWith('-')).map((a) => a.text)
      const operands = args.filter((a) => !a.text.startsWith('-'))
      if (name === 'cd' || name === 'pushd' || name === 'popd') {
        // Followed only when it surely applies to what comes next: a literal
        // directory that exists, not in a pipeline or the background (a
        // subshell), not before `||` (that runs if it failed).
        const to = operands[0]
        const dir = to && to.known && cwd !== null && name === 'cd' ? where(cwd, to.text) : null
        const sure = dir !== null && isDir(dir) && !piped && after !== '&' && after !== '||'
        cwd = sure ? dir : null
        continue
      }
      if (ALWAYS.has(name) || /^mkfs\./.test(name)) return ask
      if (name === 'eval') {
        // Read like any command line, when its text is known.
        if (args.length === 0) continue
        if (cwd === null || args.some((a) => !a.known)) return ask
        const verdict = check(args.map((a) => a.text).join(' '), cwd)
        if (verdict.ask) return ask
        creates.push(...verdict.creates)
        continue
      }
      if (DELETERS.has(name)) {
        if (viaXargs || operands.length === 0 || operands.some(deletes)) return ask
        continue
      }
      if (viaXargs && ['mv', 'cp', 'dd', 'tee', 'git'].includes(name)) return ask
      if (SHELLS.has(name)) {
        const script = args.findIndex((a) => a.text === '-c' || /^-\w*c$/.test(a.text))
        if (script >= 0) {
          const body = args[script + 1]
          if (!body || !body.known || cwd === null) return ask
          const inner = check(body.text, cwd)
          if (inner.ask) return ask
          creates.push(...inner.creates)
        }
        continue
      }
      if (name === 'find') {
        const texts = args.map((a) => a.text)
        if (texts.includes('-delete')) return ask
        const exec = texts.findIndex((t) => ['-exec', '-execdir', '-ok', '-okdir'].includes(t))
        if (exec >= 0 && DELETERS.has(basename(texts[exec + 1] ?? ''))) return ask
        continue
      }
      if ((name === 'chmod' || name === 'chown') && flags.some((f) => /^-\w*R/.test(f) || f === '--recursive')) return ask
      if (name === 'dd') {
        const of = args.find((a) => a.text.startsWith('of='))
        if (of && target({ text: of.text.slice(3), known: of.known })) return ask
        continue
      }
      if (name === 'tee') {
        if (!flags.some((f) => f === '-a' || f === '--append') && operands.some(target)) return ask
        continue
      }
      if (name === 'mv' || name === 'cp') {
        const into = args.findIndex((a) => a.text === '-t' || a.text === '--target-directory')
        const dir = into >= 0 ? args[into + 1] : undefined
        const sources = dir ? operands.filter((a) => a !== dir) : operands.slice(0, -1)
        const dest = dir ?? operands.at(-1)
        if (!dest || sources.length === 0) continue
        if (!dest.known || cwd === null) return ask
        const destPath = where(cwd, dest.text)
        const landings =
          dir || isDir(destPath)
            ? sources.map((s) => ({ text: resolve(destPath, basename(s.text)), known: s.known }))
            : [{ text: destPath, known: true }]
        if (landings.some(target)) return ask
        continue
      }
      if (name === 'git' && gitRisky(args, cwd)) return ask
    }
    return { ask: false, creates }
  }

  function gitRisky(args: Word[], cwd: string | null): boolean {
    let i = 0
    while (i < args.length && args[i].text.startsWith('-')) {
      if (['-C', '-c', '--git-dir', '--work-tree', '--namespace'].includes(args[i].text)) i++
      i++
    }
    const sub = args[i]?.text ?? ''
    const rest = args.slice(i + 1).map((a) => a.text)
    const flags = new Set(rest.filter((a) => a.startsWith('-')))
    const paths = rest.filter((a) => !a.startsWith('-'))
    switch (sub) {
      case 'reset':
        return flags.has('--hard') || flags.has('--merge') || flags.has('--keep')
      case 'clean':
        return !flags.has('-n') && !flags.has('--dry-run')
      case 'rm':
        return !flags.has('--cached')
      case 'restore':
        // Unstaging only touches the index; anything else rewrites files.
        return !(flags.has('--staged') && !flags.has('--worktree') && !flags.has('-W'))
      case 'checkout':
        // A branch switch is fine; checking out paths discards their changes.
        return (
          flags.has('-f') ||
          flags.has('--force') ||
          rest.includes('--') ||
          paths.includes('.') ||
          (cwd !== null && paths.some((p) => owners(where(cwd, p))))
        )
      case 'switch':
        return flags.has('-f') || flags.has('--force') || flags.has('--discard-changes')
      case 'branch':
        return flags.has('-D') || (flags.has('-d') && flags.has('--force'))
      case 'push':
        return (
          [...flags].some((f) => f === '-f' || f.startsWith('--force') || f === '--mirror' || f === '--delete' || f === '-d') ||
          paths.some((p) => p.startsWith('+') || p.startsWith(':'))
        )
      case 'stash':
        return paths[0] === 'drop' || paths[0] === 'clear'
      default:
        return false
    }
  }

  pi.on('tool_call', async (event: any, ctx: any) => {
    const cwd = ctx.cwd ?? process.cwd()
    let subject: string | false = false
    let creates: string[] = []
    if (event.toolName === 'bash') {
      const command = String(event.input?.command ?? '')
      const verdict = check(command, cwd)
      creates = verdict.creates
      if (verdict.ask) subject = `Run \`${command}\`?`
    } else if (['write', 'edit'].includes(event.toolName)) {
      const file = String(event.input?.path ?? '')
      const path = where(cwd, file)
      if (PROTECTED.test(file)) subject = `Change the protected file ${file}?`
      else if (event.toolName === 'write' && owners(path)) subject = `Overwrite the existing file ${file}?`
      else if (event.toolName === 'write' && !existsSync(path)) creates = [path]
    }
    if (subject) {
      if (!ctx.hasUI) return { block: true, reason: 'needs confirmation, and no one is there to ask' }
      const choice = await ctx.ui.select(subject, ['Allow', 'Block'])
      if (choice !== 'Allow') return { block: true, reason: 'the user blocked it' }
    }
    // Only once it may run: the files it makes are then the agent's own.
    for (const path of creates) created.add(path)
    return undefined
  })
}
