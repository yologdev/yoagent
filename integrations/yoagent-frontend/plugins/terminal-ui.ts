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
// The answer parsing the page's tests cover (`node --test web/`).
import { parseAnswer } from '../web/lib.js'
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

/** The title a tool's view (`details.view = {call?, result?}`) gives its call. */
const viewTitle = (details: any): string | undefined => {
  const title = details?.view?.result?.title ?? details?.view?.call?.title
  return typeof title === 'string' && title ? short(title, 60) : undefined
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
    // Plugin questions waiting for an answer, oldest first; the editor
    // answers the first.
    const questions: { id: number; request: Message }[] = []
    const say = (text: string) => log.addChild(new Text(text, 1, 0))
    const send = (message: Record<string, unknown>) =>
      frontend.send(message).catch((e: unknown) => {
        say(red(`could not send: ${short(String(e))}`))
        tui.requestRender()
      })
    // The screen is gone by then: a failed quit is told on stderr.
    const quit = () => {
      tui.stop()
      frontend
        .send({ type: 'quit' })
        .catch((e: unknown) => process.stderr.write(`yoagent: could not quit the session: ${String(e)}\n`))
    }
    // Text an `input` question put in the editor, cleared if the question
    // closes before the user answers.
    let prefilled: string | undefined

    const showQuestion = () => {
      const first = questions[0]
      if (!first) {
        status.setText(running ? yellow('working…') : idle)
        return
      }
      const r = first.request
      say(bold(`  ? ${r.title}`) + (r.message ? dim(` — ${r.message}`) : ''))
      if (r.kind === 'select') r.options.forEach((o: string, i: number) => say(`    ${i + 1}. ${o}`))
      if (r.kind === 'input' && r.value) {
        editor.setText(r.value)
        prefilled = r.value
      }
      status.setText(
        yellow(
          r.kind === 'confirm'
            ? 'answer y or n'
            : r.kind === 'select'
              ? r.multiple
                ? 'answer with numbers, e.g. 1,3 (empty for none)'
                : 'answer with a number (empty for no choice)'
              : 'type your answer',
        ),
      )
    }
    const askUser = (id: number, r: Message) => {
      if (r.kind === 'notify') return say(yellow(`  ! ${r.message}`))
      if (headless) {
        // Nobody to ask: decline, as a user who walked away would.
        say(bold(`  ? ${r.title}`) + (r.message ? dim(` — ${r.message}`) : ''))
        say(dim('  answered: null (headless demo)'))
        send({ type: 'uiResponse', id, value: r.kind === 'confirm' ? false : null })
        return
      }
      if (questions.some((q) => q.id === id)) return
      questions.push({ id, request: r })
      if (questions.length === 1) showQuestion()
    }

    const answerQuestion = (text: string) => {
      const first = questions[0]
      if (!first) return false
      const { id, request } = first
      const parsed = parseAnswer(request, text)
      if ('error' in parsed) {
        // Not an answer: ask again rather than guess.
        say(red(`  ${parsed.error}`))
        return true
      }
      const { value } = parsed
      say(dim(`  answered: ${JSON.stringify(value)}`))
      prefilled = undefined
      questions.shift()
      send({ type: 'uiResponse', id, value })
      showQuestion()
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
      else quit()
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
          // A tool's view (`details.view`) names the call better than its tool name.
          tools
            .get(e.toolCallId)
            ?.setText(`${e.isError ? red('  ▶ ') : green('  ▶ ')}${viewTitle(e.result?.details) ?? e.toolName} ${e.isError ? red('✗') : green('✓')}`)
          tools.delete(e.toolCallId)
          return
        case 'providerRetry':
          say(dim('  (retrying the model request)'))
      }
    }

    const outcomeLine = (m: Message) => {
      switch (m.outcome) {
        case 'completed':
          return undefined
        case 'aborted':
          return yellow('  stopped')
        case 'rejected':
          return red(`  refused: ${m.error ?? 'input rejected'}`)
        default:
          return red(`  failed: ${m.error ?? 'unknown error'}`)
      }
    }

    const handle = (message: Message) => {
      switch (message.type) {
        case 'runStart':
          running = true
          say(cyan('› ') + message.prompt)
          status.setText(yellow('working…') + dim(' Ctrl+C to stop'))
          break
        case 'event':
          onEvent(message.event)
          break
        case 'runEnd': {
          running = false
          answer = undefined
          const line = outcomeLine(message)
          if (line) say(line)
          status.setText(idle)
          if (headless) {
            const lines = [header, log, status].flatMap((c) => c.render(100))
            process.stdout.write(`${lines.map((l) => l.trimEnd()).join('\n')}\n`)
            quit()
          }
          break
        }
        case 'notice':
          say(message.level === 'error' ? red(`  ! ${message.message}`) : yellow(`  ! ${message.message}`))
          break
        case 'hello':
          for (const { id, request } of message.uiRequests ?? []) askUser(id, request)
          break
        case 'uiRequest':
          askUser(message.id, message.request)
          break
        case 'uiResolved': {
          const index = questions.findIndex((q) => q.id === message.id)
          if (index >= 0) {
            questions.splice(index, 1)
            const why: Record<string, string> = {
              answered: 'answered elsewhere',
              timedOut: 'timed out: the safe answer was used',
              withdrawn: 'withdrawn',
            }
            say(dim(`  (${why[message.reason] ?? 'closed'})`))
            if (index === 0) {
              if (prefilled !== undefined && editor.getText() === prefilled) editor.setText('')
              prefilled = undefined
              showQuestion()
            }
          }
          break
        }
        case 'closed':
          tui.stop()
          if (!headless) process.stderr.write('yoagent: the session ended\n')
      }
    }

    // A message this frontend fails on is shown, not thrown: a throw would
    // disconnect it from the session.
    const receive = async (message: Message) => {
      try {
        handle(message)
      } catch (e) {
        say(red(`  (could not show a ${short(String(message?.type), 20)} message: ${short(String(e))})`))
      }
      tui.requestRender()
    }

    ctx.effect(frontend.connect({ receive }))
    ctx.effect(() => tui.stop())
    tui.start()
    if (headless) send({ type: 'prompt', text: config.demo! })
  },
})
