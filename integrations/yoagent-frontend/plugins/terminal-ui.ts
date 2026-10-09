// EXPERIMENTAL. A terminal frontend built from pi's UI library (pi-tui), on
// the yoagent-frontend protocol: it connects to the host's `frontend`
// service, renders what arrives (`receive`), and sends what the user types.
// Plugins' questions (`uiRequest`) are answered in the editor.
//
// Config: `demo` runs that prompt headless — rendered into memory, the
// screen printed when the run ends — instead of reading the keyboard.
// rutis starts Node with stdin closed, so the keyboard comes from /dev/tty
// (macOS / Linux).

import * as fs from 'node:fs'
import * as tty from 'node:tty'
import { definePlugin } from '@arcships/rutis'
import {
  Container,
  Editor,
  Markdown,
  ProcessTerminal,
  Text,
  TuiMainScreen,
  matchesKey,
  type Terminal,
} from '@earendil-works/pi-tui'

interface Frontend {
  connect(client: { receive(message: Message): Promise<void> }): () => void
  send(message: Record<string, unknown>): Promise<void>
}

type Message = { type: string; [field: string]: any }

const ansi = (code: string) => (text: string) => `\x1b[${code}m${text}\x1b[0m`
const [dim, bold, cyan, green, red, yellow] = ['2', '1', '36', '32', '31', '33'].map(ansi)

const markdownTheme = {
  heading: bold, link: cyan, linkUrl: dim, code: yellow, codeBlock: (t: string) => t,
  codeBlockBorder: dim, quote: dim, quoteBorder: dim, hr: dim, listBullet: cyan,
  bold, italic: ansi('3'), strikethrough: ansi('9'), underline: ansi('4'),
}
const editorTheme = {
  borderColor: dim,
  selectList: { selectedPrefix: cyan, selectedText: bold, description: dim, scrollInfo: dim, noMatch: dim },
}

/** Renders into memory: the headless `demo` mode. */
class MemoryTerminal implements Terminal {
  start() {}
  stop() {}
  async drainInput() {}
  write() {}
  get columns() { return 100 }
  get rows() { return 40 }
  get kittyProtocolActive() { return false }
  moveBy() {}
  hideCursor() {}
  showCursor() {}
  clearLine() {}
  clearFromCursor() {}
  clearScreen() {}
  setTitle() {}
  setProgress() {}
  setProgramStatus() {}
}

const short = (value: unknown, max = 70) => {
  const text = typeof value === 'string' ? value : JSON.stringify(value)
  return text.length > max ? `${text.slice(0, max)}…` : text
}

export default definePlugin<{ demo?: string }>({
  inject: ['frontend'],
  config: { type: 'object', properties: { demo: { type: 'string' } } },
  apply(ctx, config) {
    const frontend = ctx.use<Frontend>('frontend')
    const headless = config.demo !== undefined
    let terminal: Terminal
    if (headless) terminal = new MemoryTerminal()
    else {
      const keyboard = new tty.ReadStream(fs.openSync('/dev/tty', 'r'))
      Object.defineProperty(process, 'stdin', { value: keyboard, configurable: true })
      terminal = new ProcessTerminal()
    }
    const tui = new TuiMainScreen(terminal)
    const header = new Text(bold('yoagent') + dim(' · terminal frontend · same session a browser can join'), 1, 1)
    const log = new Container()
    const idle = dim('Enter to send · Ctrl+C stops a run, or quits when idle')
    const status = new Text(idle, 1, 0)
    const editor = new Editor(tui, editorTheme)
    for (const c of [header, log, status, editor]) tui.addChild(c)
    tui.setFocus(editor)

    let running = false
    let answer: Markdown | undefined
    let answerText = ''
    const tools = new Map<string, Text>()
    // The plugin question waiting for an answer from the editor, if any.
    let question: { id: number; request: Message } | undefined
    const say = (text: string) => log.addChild(new Text(text, 1, 0))
    const send = (message: Record<string, unknown>) =>
      frontend.send(message).catch((e: unknown) => say(red(`could not send: ${short(String(e))}`)))

    const answerQuestion = (text: string) => {
      if (!question) return false
      const { id, request } = question
      let value: unknown = null
      if (request.kind === 'confirm') value = /^(y|yes)$/i.test(text.trim())
      else if (request.kind === 'select') {
        const n = Number(text.trim())
        value = Number.isInteger(n) && n >= 1 && n <= request.options.length ? request.options[n - 1] : null
      } else value = text
      say(dim(`  answered: ${JSON.stringify(value)}`))
      question = undefined
      status.setText(running ? yellow('working…') : idle)
      send({ type: 'uiResponse', id, value })
      return true
    }

    editor.onSubmit = (text) => {
      editor.setText('')
      if (answerQuestion(text)) return tui.requestRender()
      if (!text.trim()) return
      // While a run is in progress the session queues it.
      send({ type: 'prompt', text })
    }
    tui.addInputListener((data) => {
      if (!matchesKey(data, 'ctrl+c') && !matchesKey(data, 'ctrl+d')) return undefined
      if (running && matchesKey(data, 'ctrl+c')) send({ type: 'abort' })
      else {
        tui.stop()
        send({ type: 'quit' })
      }
      return { consume: true }
    })

    const onEvent = (e: Message) => {
      switch (e.type) {
        case 'messageUpdate':
          if (e.delta?.type !== 'text') return
          if (!answer) {
            answerText = ''
            answer = new Markdown('', 1, 0, markdownTheme)
            log.addChild(answer)
          }
          answerText += e.delta.delta
          answer.setText(answerText)
          return
        case 'messageEnd':
          answer = undefined
          return
        case 'toolExecutionStart': {
          const line = new Text(yellow('  ▶ ') + `${e.toolName} ${dim(short(e.args))}`, 1, 0)
          tools.set(e.toolCallId, line)
          log.addChild(line)
          return
        }
        case 'toolExecutionEnd':
          tools.get(e.toolCallId)?.setText(`${e.isError ? red('  ▶ ') : green('  ▶ ')}${e.toolName} ${e.isError ? red('✗') : green('✓')}`)
          tools.delete(e.toolCallId)
          return
        case 'providerRetry':
          say(dim('  (retrying the model request)'))
      }
    }

    const receive = async (message: Message) => {
      switch (message.type) {
        case 'runStarted':
          running = true
          say(cyan('› ') + message.prompt)
          status.setText(yellow('working…') + dim(' Ctrl+C to stop'))
          break
        case 'event':
          onEvent(message.event)
          break
        case 'runEnded':
          running = false
          answer = undefined
          if (message.error) say(red(`  run ended: ${message.error}`))
          status.setText(idle)
          if (headless) {
            const lines = [header, log, status].flatMap((c) => c.render(100))
            process.stdout.write(`${lines.map((l) => l.trimEnd()).join('\n')}\n`)
            tui.stop()
            send({ type: 'quit' })
          }
          break
        case 'uiRequest': {
          const r = message.request
          if (r.kind === 'notify') {
            say(yellow(`  ! ${r.message}`))
            break
          }
          say(bold(`  ? ${r.title}`) + (r.message ? dim(` — ${r.message}`) : ''))
          if (headless) {
            // Nobody to ask: decline, as a user who walked away would.
            say(dim('  answered: null (headless demo)'))
            send({ type: 'uiResponse', id: message.id, value: r.kind === 'confirm' ? false : null })
            break
          }
          question = { id: message.id, request: r }
          if (r.kind === 'select') r.options.forEach((o: string, i: number) => say(`    ${i + 1}. ${o}`))
          status.setText(
            yellow(r.kind === 'confirm' ? 'answer y or n' : r.kind === 'select' ? 'answer with a number' : 'type your answer'),
          )
          break
        }
        case 'uiResolved':
          if (question?.id === message.id) {
            question = undefined
            say(dim('  (answered elsewhere, or timed out)'))
            status.setText(running ? yellow('working…') : idle)
          }
          break
        case 'closed':
          tui.stop()
      }
      tui.requestRender()
    }

    ctx.effect(frontend.connect({ receive }))
    ctx.effect(() => tui.stop())
    tui.start()
    if (headless) send({ type: 'prompt', text: config.demo! })
  },
})
