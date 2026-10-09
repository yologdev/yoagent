// A coding-agent frontend built from pi's terminal UI library (pi-tui), as a
// rutis plugin: the agent loop is yoagent, in the Rust host. The plugin
// renders the run from yoagent's events (`on_event`) and sends what the user
// types through the host's `chat` service (`prompt`, `abort`, `quit`).
//
// Config:
// - `demo`: run this prompt headless instead of reading the keyboard — the
//   screen is rendered into memory and printed once the run ends (CI, demos
//   without a terminal).
//
// rutis starts the Node runtime with stdin closed, so the interactive mode
// reads the keyboard from `/dev/tty` (Unix).

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
import type { AgentEvent, Yoagent } from '../yoagent.d.ts'

interface Chat {
  prompt(text: string): Promise<void>
  abort(): Promise<void>
  quit(): Promise<void>
}

interface Config {
  demo?: string
}

const ansi = (code: string) => (text: string) => `\x1b[${code}m${text}\x1b[0m`
const dim = ansi('2')
const bold = ansi('1')
const cyan = ansi('36')
const green = ansi('32')
const red = ansi('31')
const yellow = ansi('33')

const markdownTheme = {
  heading: bold,
  link: cyan,
  linkUrl: dim,
  code: yellow,
  codeBlock: (t: string) => t,
  codeBlockBorder: dim,
  quote: dim,
  quoteBorder: dim,
  hr: dim,
  listBullet: cyan,
  bold,
  italic: ansi('3'),
  strikethrough: ansi('9'),
  underline: ansi('4'),
}

const editorTheme = {
  borderColor: dim,
  selectList: {
    selectedPrefix: cyan,
    selectedText: bold,
    description: dim,
    scrollInfo: dim,
    noMatch: dim,
  },
}

/** A terminal that renders into memory: the headless `demo` mode. */
class MemoryTerminal implements Terminal {
  out = ''
  start() {}
  stop() {}
  async drainInput() {}
  write(data: string) {
    this.out += data
  }
  get columns() {
    return 100
  }
  get rows() {
    return 40
  }
  get kittyProtocolActive() {
    return false
  }
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

export default definePlugin<Config>({
  inject: ['yoagent', 'chat'],
  config: { type: 'object', properties: { demo: { type: 'string' } } },
  apply(ctx, config) {
    const yoagent = ctx.use<Yoagent>('yoagent')
    const chat = ctx.use<Chat>('chat')
    const headless = config.demo !== undefined

    let terminal: Terminal
    if (headless) {
      terminal = new MemoryTerminal()
    } else {
      // pi-tui's ProcessTerminal reads process.stdin: give it the terminal.
      const keyboard = new tty.ReadStream(fs.openSync('/dev/tty', 'r'))
      Object.defineProperty(process, 'stdin', { value: keyboard, configurable: true })
      terminal = new ProcessTerminal()
    }
    const tui = new TuiMainScreen(terminal)
    const log = new Container()
    const status = new Text(dim('Enter to send · Ctrl+C stops a run, or quits when idle'), 1, 0)
    const editor = new Editor(tui, editorTheme)
    const header = new Text(bold('yoagent') + dim(' · loop in Rust · UI from pi-tui · plugins via rutis'), 1, 1)
    tui.addChild(header)
    tui.addChild(log)
    tui.addChild(status)
    tui.addChild(editor)
    tui.setFocus(editor)

    let running = false
    let answer: Markdown | undefined
    let answerText = ''
    const tools = new Map<string, Text>()
    const say = (text: string) => {
      log.addChild(new Text(text, 1, 0))
      tui.requestRender()
    }
    const send = (text: string) => {
      if (!text.trim() || running) return
      say(cyan('› ') + text)
      running = true
      status.setText(yellow('working…') + dim(' Ctrl+C to stop'))
      chat.prompt(text).catch((e: unknown) => say(red(`could not send: ${short(String(e))}`)))
    }
    editor.onSubmit = (text) => {
      editor.setText('')
      send(text)
    }
    tui.addInputListener((data) => {
      if (!matchesKey(data, 'ctrl+c') && !matchesKey(data, 'ctrl+d')) return undefined
      if (running && matchesKey(data, 'ctrl+c')) {
        chat.abort().catch(() => {})
        // Don't wait on `agentEnd` to unlock the editor: event delivery is
        // best-effort, and the host queues a prompt sent as a run winds down.
        running = false
        status.setText(dim('stopped · Enter to send · Ctrl+C again quits'))
      } else {
        tui.stop()
        chat.quit().catch(() => {})
      }
      return { consume: true }
    })

    const onEvent = async (event: AgentEvent) => {
      const e = event as AgentEvent & Record<string, any>
      switch (e.type) {
        case 'messageUpdate':
          if (e.delta?.type !== 'text') break
          if (!answer) {
            answerText = ''
            answer = new Markdown('', 1, 0, markdownTheme)
            log.addChild(answer)
          }
          answerText += e.delta.delta
          answer.setText(answerText)
          break
        case 'messageEnd':
          answer = undefined
          break
        case 'toolExecutionStart': {
          const line = new Text(yellow('  ▶ ') + `${e.toolName} ${dim(short(e.args))}`, 1, 0)
          tools.set(e.toolCallId, line)
          log.addChild(line)
          break
        }
        case 'toolExecutionEnd': {
          const line = tools.get(e.toolCallId)
          const mark = e.isError ? red('✗') : green('✓')
          line?.setText(`${e.isError ? red('  ▶ ') : green('  ▶ ')}${e.toolName} ${mark}`)
          tools.delete(e.toolCallId)
          break
        }
        case 'providerRetry':
          say(dim('  (retrying the model request)'))
          break
        case 'agentEnd':
          running = false
          status.setText(dim('Enter to send · Ctrl+C stops a run, or quits when idle'))
          if (headless) {
            // The final screen, rendered once from the components (the live
            // terminal stream is incremental), then stop the host.
            const lines = [header, log, status].flatMap((c) => c.render(100))
            process.stdout.write(`${lines.map((l) => l.trimEnd()).join('\n')}\n`)
            tui.stop()
            chat.quit().catch(() => {})
          }
          break
      }
      tui.requestRender()
    }

    ctx.effect(
      yoagent.register(
        'pi-tui-frontend',
        { on_event: onEvent },
        {
          events: [
            'messageUpdate',
            'messageEnd',
            'toolExecutionStart',
            'toolExecutionEnd',
            'providerRetry',
            'agentEnd',
          ],
        },
      ),
    )
    ctx.effect(() => tui.stop())
    tui.start()
    if (headless) send(config.demo!)
  },
})
