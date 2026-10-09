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
