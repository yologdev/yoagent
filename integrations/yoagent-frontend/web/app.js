// The browser frontend: the frontend protocol over a WebSocket, plus UI
// plugins — ES modules plugins offer, loaded from /ui-plugins/<name>.js:
//
//   export function renderTool({ toolName, args, result, isError }, element) {}
//   export function mountPanel(element, { send }) {}
//
// `renderTool` draws a tool's result under its line (for the tools the plugin
// listed); `mountPanel` gets a side panel and a way to send protocol messages.

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
const send = (message) => socket.send(JSON.stringify(message))

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
// Minimal Markdown for answers: **bold**, `code`, [text](url); everything
// else stays text. Built as DOM nodes, never as HTML from the model.
function markdown(text, element) {
  element.replaceChildren()
  const pattern = /(\*\*[^*]+\*\*|`[^`]+`|\[[^\]]+\]\(https?:\/\/[^)\s]+\))/g
  let last = 0
  for (const match of text.matchAll(pattern)) {
    element.append(text.slice(last, match.index))
    const token = match[0]
    let node
    if (token.startsWith('**')) {
      node = document.createElement('strong')
      node.textContent = token.slice(2, -2)
    } else if (token.startsWith('`')) {
      node = document.createElement('code')
      node.textContent = token.slice(1, -1)
    } else {
      const [, label, url] = token.match(/^\[([^\]]+)\]\(([^)]+)\)$/)
      node = document.createElement('a')
      node.href = url
      node.target = '_blank'
      node.rel = 'noreferrer'
      node.textContent = label
    }
    element.append(node)
    last = match.index + token.length
  }
  element.append(text.slice(last))
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

function renderTool(event, line) {
  for (const { info, module } of plugins.values()) {
    if (!info.tools.includes(event.toolName) || !module.renderTool) continue
    const el = document.createElement('div')
    el.className = 'rendered'
    line.after(el)
    try {
      module.renderTool(
        { toolName: event.toolName, args: tools.get(event.toolCallId)?.args, result: event.result, isError: event.isError },
        el,
      )
    } catch (error) {
      el.textContent = `(${info.name} could not render this: ${error})`
    }
  }
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
      line.textContent = `▶ ${event.toolName} ${short(event.args)}`
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

// Questions wait in order: one dialog at a time, the next when it closes.
const questions = []

function ask(id, request) {
  if (request.kind === 'notify') {
    add('notice', request.message)
    return
  }
  if (questions.some((q) => q.id === id)) return
  questions.push({ id, request })
  showNextQuestion()
}

function resolved(id) {
  const index = questions.findIndex((q) => q.id === id)
  if (index < 0) return
  questions.splice(index, 1)
  if (dialog.open && dialog.dataset.id === String(id)) {
    dialog.close()
    showNextQuestion()
  }
}

function showNextQuestion() {
  if (dialog.open || questions.length === 0) return
  const { id, request } = questions[0]
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
  const reply = (value) => {
    send({ type: 'uiResponse', id: Number(id), value })
    resolved(id)
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
socket.onclose = () =>
  (status.textContent = token ? 'disconnected' : 'not connected: open the URL the server printed (it carries a token)')
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
    case 'runStarted':
      add('you', `› ${message.prompt}`)
      status.textContent = 'working…'
      return
    case 'event':
      onEvent(message.event)
      return
    case 'runEnded': {
      const s = message.stats || {}
      const cost = s.totalCostUsd != null ? ` · $${Number(s.totalCostUsd).toFixed(4)}` : ''
      add('end', message.error ? `run ${message.run} ended: ${message.error}` : `run ${message.run} done${cost}`)
      status.textContent = 'ready'
      answer = null
      return
    }
    case 'uiRequest':
      ask(message.id, message.request)
      return
    case 'uiResolved':
      resolved(message.id)
      return
    case 'closed':
      status.textContent = 'session closed'
  }
}

document.getElementById('composer').onsubmit = (e) => {
  e.preventDefault()
  const text = input.value.trim()
  if (!text) return
  send({ type: 'prompt', text })
  input.value = ''
}
input.onkeydown = (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault()
    document.getElementById('composer').requestSubmit()
  }
}
document.getElementById('stop').onclick = () => send({ type: 'abort' })
