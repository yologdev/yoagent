// The page's logic that needs no DOM, shared by app.js and its tests
// (`node --test web/`).

// Minimal Markdown for answers: **bold**, `code`, [text](https://…); the
// rest is text. Returns parts the page builds as DOM nodes — never HTML from
// the model. Only https links become links.
export function markdownParts(text) {
  const parts = []
  const pattern = /(\*\*[^*]+\*\*|`[^`]+`|\[[^\]]+\]\(https:\/\/[^)\s]+\))/g
  let last = 0
  for (const match of text.matchAll(pattern)) {
    if (match.index > last) parts.push({ kind: 'text', text: text.slice(last, match.index) })
    const token = match[0]
    if (token.startsWith('**')) {
      parts.push({ kind: 'strong', text: token.slice(2, -2) })
    } else if (token.startsWith('`')) {
      parts.push({ kind: 'code', text: token.slice(1, -1) })
    } else {
      const [, label, url] = token.match(/^\[([^\]]+)\]\(([^)]+)\)$/)
      parts.push({ kind: 'link', text: label, url })
    }
    last = match.index + token.length
  }
  if (last < text.length) parts.push({ kind: 'text', text: text.slice(last) })
  return parts
}

// Questions wait in order: one shown at a time, oldest first. A question
// may arrive twice (in `hello` and as its own `uiRequest`): kept once.
export class Questions {
  constructor() {
    this.waiting = []
  }

  // Queue a question; returns whether it was new.
  add(id, request) {
    if (this.waiting.some((q) => q.id === id)) return false
    this.waiting.push({ id, request })
    return true
  }

  // Drop a question (answered here or elsewhere, withdrawn, timed out);
  // returns whether it was the one shown.
  remove(id) {
    const index = this.waiting.findIndex((q) => q.id === id)
    if (index < 0) return false
    this.waiting.splice(index, 1)
    return index === 0
  }

  // The question to show: the oldest.
  current() {
    return this.waiting[0]
  }

  clear() {
    this.waiting = []
  }
}

// The line for a finished run.
export function runEndLine({ run, outcome, error, totalCostUsd }) {
  const cost = totalCostUsd != null ? ` · $${Number(totalCostUsd).toFixed(4)}` : ''
  switch (outcome) {
    case 'completed':
      return `run ${run} done${cost}`
    case 'aborted':
      return `run ${run} stopped${cost}`
    case 'rejected':
      return `run ${run} refused: ${error ?? 'input rejected'}`
    default:
      return `run ${run} failed: ${error ?? 'unknown error'}${cost}`
  }
}

// Tool cards. A tool result may carry `details.view = {call?, result?}`: how
// the tool wants its call shown, in a small card vocabulary (the DeepSeek
// Harness one): generic, terminal, diff, search, read, web. Anything else is
// ignored, and the page falls back to the plain tool line.
const CARDS = new Set(['generic', 'terminal', 'diff', 'search', 'read', 'web'])

export function toolView(details) {
  const view = details?.view
  if (!view || typeof view !== 'object') return undefined
  const card = (c) => (c && typeof c === 'object' && CARDS.has(c.card) ? c : undefined)
  const call = card(view.call)
  const result = card(view.result)
  return call || result ? { call, result } : undefined
}

// The title to show for the call: the result's, else the call's.
export function viewTitle(view) {
  const title = view?.result?.title ?? view?.call?.title
  return typeof title === 'string' && title ? title : undefined
}

// A line diff (longest common subsequence): rows of `{op, text}`, op ' ', '-'
// or '+'. `oldText` null is a new file. When the old and new line counts
// multiplied exceed `max`², it does not align: every old line removed, every
// new one added.
export function diffRows(oldText, newText, max = 400) {
  const a = oldText == null ? [] : String(oldText).split('\n')
  const b = String(newText ?? '').split('\n')
  if (a.length * b.length > max * max) {
    return [...a.map((text) => ({ op: '-', text })), ...b.map((text) => ({ op: '+', text }))]
  }
  const lcs = Array.from({ length: a.length + 1 }, () => new Uint32Array(b.length + 1))
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1])
    }
  }
  const rows = []
  let i = 0
  let j = 0
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      rows.push({ op: ' ', text: a[i] })
      i++
      j++
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) rows.push({ op: '-', text: a[i++] })
    else rows.push({ op: '+', text: b[j++] })
  }
  while (i < a.length) rows.push({ op: '-', text: a[i++] })
  while (j < b.length) rows.push({ op: '+', text: b[j++] })
  return rows
}

// A link target the page may use: http(s) only.
export function safeUrl(url) {
  try {
    const parsed = new URL(String(url))
    return parsed.protocol === 'https:' || parsed.protocol === 'http:' ? parsed.href : undefined
  } catch {
    return undefined
  }
}

// A typed answer (the terminal UI) to a question: `{ value }`, or `{ error }`
// when it does not fit, so the question is asked again rather than answered
// with a guess. confirm: y/yes or n/no. select: a number (blank: no choice);
// with `multiple`, numbers split by commas or spaces, each at most once
// (blank: none). input: the text.
export function parseAnswer(request, text) {
  const trimmed = String(text ?? '').trim()
  if (request.kind === 'confirm') {
    if (/^(y|yes)$/i.test(trimmed)) return { value: true }
    if (/^(n|no)$/i.test(trimmed)) return { value: false }
    return { error: 'answer y or n' }
  }
  if (request.kind === 'select') {
    const options = request.options ?? []
    const pick = (token) => {
      const n = Number(token)
      return /^\d+$/.test(token) && n >= 1 && n <= options.length ? options[n - 1] : undefined
    }
    if (request.multiple) {
      const tokens = trimmed.split(/[\s,]+/).filter(Boolean)
      const bad = tokens.find((token) => pick(token) === undefined)
      if (bad !== undefined) return { error: `"${bad}" is not one of 1–${options.length}` }
      return { value: [...new Set(tokens.map(pick))] }
    }
    if (!trimmed) return { value: null }
    const choice = pick(trimmed)
    return choice === undefined ? { error: `answer with a number from 1 to ${options.length}` } : { value: choice }
  }
  return { value: String(text ?? '') }
}
