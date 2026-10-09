// The browser frontend: the frontend protocol over a WebSocket, plus UI
// plugins — ES modules plugins offer, loaded from /ui-plugins/<name>.js:
//
//   export function renderTool({ toolName, args, result, isError }, element) {}
//   export function mountPanel(element, { send }) {}
//
// `renderTool` draws a tool's result under its line (for the tools the plugin
// listed); `mountPanel` gets a side panel and a way to send protocol messages.
// A tool no plugin draws, whose result carries `details.view`, gets a card.

import { diffRows, markdownParts, Questions, runEndLine, safeUrl, toolView, viewTitle } from './lib.js'

const log = document.getElementById('log')
const panels = document.getElementById('panels')
const status = document.getElementById('status')
const input = document.getElementById('input')
const dialog = document.getElementById('dialog')

// The server's token, from this page's URL: `/ws` refuses connections without it.
const token = new URLSearchParams(location.search).get('t') ?? ''
const socket = new WebSocket(
  `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws?t=${encodeURIComponent(token)}`,
)
// Whether the session still hears us: the socket is open and the session
// has not ended. A message that cannot go says so instead of vanishing.
let ended = false
function send(message) {
  if (ended || socket.readyState !== WebSocket.OPEN) {
    add('notice error', ended ? 'The session has ended: nothing more is sent.' : 'Not connected: that was not sent.')
    return false
  }
  socket.send(JSON.stringify(message))
  return true
}

const plugins = new Map() // name -> { info, module, panel }
let answer = null
let answerText = ''
const tools = new Map() // toolCallId -> { line, args }
const scroll = () => (log.scrollTop = log.scrollHeight)
const add = (className, text) => {
  const el = document.createElement('div')
  el.className = className
  if (text !== undefined) el.textContent = text
  log.append(el)
  scroll()
  return el
}
// Answers as DOM nodes built from markdownParts — never HTML from the model.
function markdown(text, element) {
  element.replaceChildren()
  for (const part of markdownParts(text)) {
    if (part.kind === 'text') {
      element.append(part.text)
      continue
    }
    const node = document.createElement(part.kind === 'link' ? 'a' : part.kind)
    node.textContent = part.text
    if (part.kind === 'link') {
      node.href = part.url
      node.target = '_blank'
      node.rel = 'noreferrer'
    }
    element.append(node)
  }
}

const short = (value, max = 80) => {
  const text = typeof value === 'string' ? value : JSON.stringify(value)
  return text.length > max ? `${text.slice(0, max)}…` : text
}

// The offered plugins, as the server lists them: withdrawn ones leave, new
// ones and re-offered ones (a new version) are (re)loaded.
async function loadPlugins(list) {
  const offered = new Map(list.map((info) => [info.name, info]))
  for (const [name, loaded] of plugins) {
    if (offered.get(name)?.version !== loaded.info.version) {
      loaded.panel?.remove()
      plugins.delete(name)
    }
  }
  for (const info of list) {
    if (plugins.has(info.name)) continue
    try {
      const module = await import(`/ui-plugins/${encodeURIComponent(info.name)}.js?v=${info.version}`)
      let panel
      if (info.panel && module.mountPanel) {
        panel = document.createElement('section')
        panels.append(panel)
        module.mountPanel(panel, { send })
      }
      plugins.set(info.name, { info, module, panel })
    } catch (error) {
      add('notice', `UI plugin ${info.name} failed to load: ${error}`)
    }
  }
}

// Lines a card shows before it says how many more there are.
const CARD_LINES = 200

const el = (tag, className, text) => {
  const node = document.createElement(tag)
  if (className) node.className = className
  if (text !== undefined) node.textContent = text
  return node
}

// A <pre> of rows, each a [text, className?], cut at CARD_LINES.
function rows(list) {
  const pre = el('pre')
  for (const [text, className] of list.slice(0, CARD_LINES)) pre.append(el('span', className, `${text}\n`))
  if (list.length > CARD_LINES) pre.append(el('span', 'more', `… ${list.length - CARD_LINES} more lines`))
  return pre
}

const texts = (content) => (Array.isArray(content) ? content.filter((b) => b?.type === 'text').map((b) => String(b.text ?? '')).join('\n') : '')

function link(url, label) {
  const href = safeUrl(url)
  if (!href) return el('span', '', String(label ?? url ?? ''))
  const a = el('a', '', String(label || href))
  a.href = href
  a.target = '_blank'
  a.rel = 'noreferrer'
  return a
}

// The card for a tool's view: what its result shows, else its call.
function card(view) {
  const { call, result } = view
  const shown = result ?? call
  const box = el('div', `card ${shown.card}`)
  const note = (text) => box.append(el('div', 'note', text))
  switch (shown.card) {
    case 'terminal': {
      const head = el('div', 'head', `$ ${call?.title ?? result?.title ?? ''}`)
      if (call?.cwd) head.append(el('span', 'note', `  in ${call.cwd}`))
      box.append(head)
      if (call?.description) note(call.description)
      if (result?.output) box.append(rows(String(result.output).replace(/\n$/, '').split('\n').map((t) => [t])))
      if (result?.exitCode !== undefined) note(`exit ${result.exitCode}`)
      else if (result?.signal) note(`killed by ${result.signal}`)
      break
    }
    case 'diff':
      for (const diff of shown.diffs ?? []) {
        box.append(el('div', 'head', diff.oldText == null ? `${diff.path} (new)` : diff.path))
        box.append(rows(diffRows(diff.oldText, diff.newText).map((r) => [`${r.op} ${r.text}`, r.op === '+' ? 'add' : r.op === '-' ? 'del' : ''])))
      }
      break
    case 'search':
      if (shown.shape === 'paths') box.append(rows((shown.paths ?? []).map((p) => [p])))
      else
        for (const file of shown.files ?? []) {
          box.append(el('div', 'head', file.path))
          box.append(rows((file.matches ?? []).map((m) => [`${m.lineNumber}: ${m.line}`])))
        }
      if (shown.truncated) note(`showing part of ${shown.total} results`)
      break
    case 'read':
      box.append(el('div', 'head', shown.path))
      box.append(rows((shown.lines ?? []).map((l) => [`${String(l.number).padStart(5)}  ${l.text}`])))
      if (shown.totalLines !== undefined) note(`${(shown.lines ?? []).length} of ${shown.totalLines} lines`)
      break
    case 'web':
      if (shown.kind === 'fetch') {
        const head = el('div', 'head')
        head.append(link(shown.url), ` · HTTP ${shown.statusCode}`)
        box.append(head)
      } else {
        if (shown.answer) box.append(el('p', '', shown.answer))
        const list = el('ol')
        for (const source of (shown.sources ?? []).slice(0, 20)) {
          const item = el('li')
          item.append(link(source.url, source.title || source.url))
          if (source.snippet) item.append(el('div', 'note', source.snippet))
          list.append(item)
        }
        box.append(list)
      }
      if (shown.truncated) note('cut to the result limit')
      break
    default: {
      const text = texts(result?.content) || texts(call?.content)
      if (!text) return undefined
      box.append(rows(text.split('\n').map((t) => [t])))
    }
  }
  return box
}

// A tool's view, under its line; the line takes the view's title.
function renderView(event, line) {
  const view = toolView(event.result?.details)
  if (!view) return
  const title = viewTitle(view)
  if (title) {
    line.title = `${event.toolName} ${short(tools.get(event.toolCallId)?.args, 400)}`
    line.firstChild.textContent = `▶ ${title}`
  }
  try {
    const box = card(view)
    if (box) line.after(box)
  } catch (error) {
    console.warn('a tool card could not be drawn', error)
  }
}

// UI plugins that draw this tool draw it; otherwise its own view, if any.
function renderTool(event, line) {
  let drawn = false
  for (const { info, module } of plugins.values()) {
    if (!info.tools.includes(event.toolName) || !module.renderTool) continue
    drawn = true
    const box = el('div', 'rendered')
    line.after(box)
    try {
      module.renderTool(
        { toolName: event.toolName, args: tools.get(event.toolCallId)?.args, result: event.result, isError: event.isError },
        box,
      )
    } catch (error) {
      box.textContent = `(${info.name} could not render this: ${error})`
    }
  }
  if (!drawn) renderView(event, line)
}

function onEvent(event) {
  switch (event.type) {
    case 'messageUpdate':
      if (event.delta?.type !== 'text') return
      if (!answer) {
        answerText = ''
        answer = add('answer')
      }
      answerText += event.delta.delta
      markdown(answerText, answer)
      scroll()
      return
    case 'messageEnd':
      answer = null
      return
    case 'toolExecutionStart': {
      const line = add('tool')
      line.append(document.createTextNode(`▶ ${event.toolName} ${short(event.args)}`))
      tools.set(event.toolCallId, { line, args: event.args })
      return
    }
    case 'toolExecutionEnd': {
      const entry = tools.get(event.toolCallId)
      if (!entry) return
      const mark = document.createElement('span')
      mark.className = event.isError ? 'bad' : 'ok'
      mark.textContent = event.isError ? ' ✗' : ' ✓'
      entry.line.append(mark)
      renderTool(event, entry.line)
      tools.delete(event.toolCallId)
      return
    }
    case 'providerRetry':
      add('notice', 'retrying the model request…')
  }
}

// One dialog at a time, the next when it closes.
const questions = new Questions()

function ask(id, request) {
  if (request.kind === 'notify') {
    add(request.level === 'error' ? 'notice error' : 'notice', request.message)
    return
  }
  if (questions.add(id, request)) showNextQuestion()
}

function resolved(id, reason) {
  const shown = questions.remove(id)
  if (reason === 'timedOut') add('notice', 'A question timed out: the safe answer was used.')
  if (shown && dialog.open) {
    dialog.close()
    showNextQuestion()
  }
}

function showNextQuestion() {
  if (dialog.open || !questions.current()) return
  const { id, request } = questions.current()
  dialog.replaceChildren()
  dialog.dataset.id = id
  const title = document.createElement('h3')
  title.textContent = request.title
  dialog.append(title)
  if (request.message) {
    const p = document.createElement('p')
    p.textContent = request.message
    dialog.append(p)
  }
  const actions = document.createElement('div')
  actions.className = 'actions'
  // Kept open when the answer cannot go: answer again once reconnected,
  // or let it time out.
  const reply = (value) => {
    if (send({ type: 'uiResponse', id, value })) resolved(id)
  }
  const button = (label, value, primary) => {
    const b = document.createElement('button')
    b.type = 'button'
    b.textContent = label
    if (primary) b.className = 'primary'
    b.onclick = () => reply(typeof value === 'function' ? value() : value)
    actions.append(b)
    return b
  }
  if (request.kind === 'confirm') {
    button('No', false)
    button('Yes', true, true).autofocus = true
  } else if (request.kind === 'select' && request.multiple) {
    const boxes = request.options.map((option) => {
      const label = el('label', 'choice')
      const box = el('input')
      box.type = 'checkbox'
      label.append(box, ` ${option}`)
      dialog.append(label)
      return [option, box]
    })
    button('Cancel', null)
    button('OK', () => boxes.filter(([, box]) => box.checked).map(([option]) => option), true)
  } else if (request.kind === 'select') {
    button('Cancel', null)
    for (const option of request.options) button(option, option, true)
  } else if (request.kind === 'input') {
    const field = document.createElement('input')
    field.placeholder = request.placeholder || ''
    field.value = request.value || ''
    dialog.append(field)
    button('Cancel', null)
    button('OK', () => field.value, true)
  }
  dialog.append(actions)
  dialog.oncancel = (e) => {
    e.preventDefault()
    reply(request.kind === 'confirm' ? false : null)
  }
  dialog.showModal()
}

socket.onopen = () => (status.textContent = 'connected')
socket.onclose = () => {
  if (ended) return
  status.textContent = token ? 'disconnected: reload to reconnect' : 'not connected: open the URL the server printed (it carries a token)'
  // Nothing can be answered from here any more.
  questions.clear()
  if (dialog.open) dialog.close()
}
// One message at a time, in order: a message waits for the previous one
// (a plugin import, say) to finish.
let handled = Promise.resolve()
socket.onmessage = (frame) => {
  const message = JSON.parse(frame.data)
  handled = handled.then(() => handle(message)).catch((error) => console.error(error))
}

async function handle(message) {
  switch (message.type) {
    case 'hello':
      status.textContent = message.running ? 'working…' : 'ready'
      await loadPlugins(message.uiPlugins)
      for (const { id, request } of message.uiRequests ?? []) ask(id, request)
      return
    case 'uiPlugins':
      await loadPlugins(message.uiPlugins)
      return
    case 'runStart':
      add('you', `› ${message.prompt}`)
      status.textContent = 'working…'
      return
    case 'event':
      onEvent(message.event)
      return
    case 'runEnd':
      add(message.outcome === 'error' ? 'end error' : 'end', runEndLine(message))
      status.textContent = 'ready'
      answer = null
      return
    case 'notice':
      add(message.level === 'error' ? 'notice error' : 'notice', message.message)
      return
    case 'uiRequest':
      ask(message.id, message.request)
      return
    case 'uiResolved':
      resolved(message.id, message.reason)
      return
    case 'closed':
      ended = true
      status.textContent = 'session closed'
      questions.clear()
      if (dialog.open) dialog.close()
      return
    default:
      // A newer server's message: nothing to show.
      return
  }
}

document.getElementById('composer').onsubmit = (e) => {
  e.preventDefault()
  const text = input.value.trim()
  if (!text) return
  // Kept in the box when it could not go.
  if (send({ type: 'prompt', text })) input.value = ''
}
input.onkeydown = (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault()
    document.getElementById('composer').requestSubmit()
  }
}
document.getElementById('stop').onclick = () => send({ type: 'abort' })
