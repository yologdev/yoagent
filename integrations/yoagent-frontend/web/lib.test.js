// node --test web/
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { markdownParts, Questions, runEndLine } from './lib.js'

test('markdown keeps everything but bold, code and https links as text', () => {
  assert.deepEqual(markdownParts('a **b** `c` [d](https://e.f/g) h'), [
    { kind: 'text', text: 'a ' },
    { kind: 'strong', text: 'b' },
    { kind: 'text', text: ' ' },
    { kind: 'code', text: 'c' },
    { kind: 'text', text: ' ' },
    { kind: 'link', text: 'd', url: 'https://e.f/g' },
    { kind: 'text', text: ' h' },
  ])
})

test('markdown makes no link of other schemes, and no HTML of anything', () => {
  for (const text of ['[x](javascript:alert(1))', '[x](http://e.f)', '<img src=x onerror=alert(1)>', '[x](data:text/html,hi)']) {
    assert.deepEqual(markdownParts(text), [{ kind: 'text', text }])
  }
  assert.deepEqual(markdownParts(''), [])
})

test('questions show oldest first, once each', () => {
  const q = new Questions()
  assert.equal(q.add(1, { kind: 'confirm' }), true)
  assert.equal(q.add(2, { kind: 'select' }), true)
  assert.equal(q.add(1, { kind: 'confirm' }), false, 'a repeat from hello')
  assert.equal(q.current().id, 1)
  assert.equal(q.remove(2), false, 'not the one shown')
  assert.equal(q.current().id, 1)
  assert.equal(q.remove(1), true)
  assert.equal(q.current(), undefined)
  assert.equal(q.remove(9), false)
})

test('a run line says how the run ended', () => {
  assert.equal(runEndLine({ run: 1, outcome: 'completed', totalCostUsd: 0.01234 }), 'run 1 done · $0.0123')
  assert.equal(runEndLine({ run: 2, outcome: 'aborted', error: 'aborted' }), 'run 2 stopped')
  assert.equal(runEndLine({ run: 3, outcome: 'rejected', error: 'input rejected: no' }), 'run 3 refused: input rejected: no')
  assert.equal(runEndLine({ run: 4, outcome: 'error', error: 'overloaded' }), 'run 4 failed: overloaded')
})

import { diffRows, safeUrl, toolView, viewTitle } from './lib.js'

test('a tool view is read only in the card vocabulary', () => {
  assert.equal(toolView(undefined), undefined)
  assert.equal(toolView({ view: { call: { card: 'dance' } } }), undefined)
  const view = toolView({ view: { call: { card: 'generic', title: 'Echo hi' }, result: { card: 'terminal', output: 'hi' } } })
  assert.equal(view.call.title, 'Echo hi')
  assert.equal(viewTitle(view), 'Echo hi', 'the result has no title of its own')
  assert.equal(viewTitle({ call: { title: 'a' }, result: { title: 'b' } }), 'b')
})

test('a diff keeps common lines and marks the rest', () => {
  const ops = (rows) => rows.map((r) => r.op + r.text).join('|')
  assert.equal(ops(diffRows('a\nb\nc', 'a\nx\nc')), ' a|-b|+x| c')
  assert.equal(ops(diffRows(null, 'new')), '+new', 'a new file')
  assert.equal(ops(diffRows('a\nb', 'a\nb\nc', 1)), '-a|-b|+a|+b|+c', 'too big to align')
})

test('only http(s) links', () => {
  assert.equal(safeUrl('https://example.com/a'), 'https://example.com/a')
  assert.equal(safeUrl('http://example.com/'), 'http://example.com/')
  for (const bad of ['javascript:alert(1)', 'data:text/html,x', 'not a url', undefined]) assert.equal(safeUrl(bad), undefined)
})
