// A DSH host for the dsh tools adapter: a stand-in DSH agent, so the many
// DSH plugins that expect one on every call (`exec.agent.session.header.cwd`,
// approvals, agent-scoped questions) work under yoagent. Load it — with the
// rows in `dsh-host.json` — only when the host wants it; without it the
// adapter's calls stay agentless, as before.
//
// What the host provides (rutis host services, shared by name):
//
//   host_session    info(): {id, generation} — the host's session; the
//                   generation changes at each reset, which ends the
//                   stand-in (plugin state does not carry over a reset)
//                   wake(text) — steering / follow-ups from a DSH plugin;
//                   the host may refuse (reported, never swallowed)
//   host_workspace  cwd() — the workspace the stand-in's session runs in
//
// One stand-in per host session generation: its id is its DSH session's id
// (`agents.register` enters it as a root), its ctx a DSH scope
// (`createScope`), its session prepared, entered and announced in DSH's own
// `sessions` with `meta.cwd` the workspace. It never runs a model: its inbox
// stays empty, `cancel` / `whenIdle` / `runMaintenance` do nothing (reported
// once), and `steer` / `followup` / `send` (waking) / `inject` go to
// `host_session.wake` as text.
//
// Turns: `turn/start` at a run's first DSH call, `turn/end` at its `finish`
// (DSH's approvals need an open turn). A turn left open (a cancel, a reset,
// a lost `finish`) is closed (`aborted`) before the next one starts, and when
// the generation ends.
//
// The session's events are written to `sessionPersistence` (DSH's jsonl
// backend, a row in `dsh-host.json`) at each turn's end: `sessions.create`
// persists nothing on its own — DSH's agent loop does that, and there is
// none here. The transcript stays empty: no yoagent conversation is
// mirrored into it.
//
// The tools adapter finds this through the `yoagent-dsh-host` service it
// provides (looked up per call, not injected), and `host-dialogs.ts` answers
// DSH's `approval/request` and the stand-in's questions through the host's
// `ui`.

import { createScope } from '@deepseek-ai/dsh-scope'
import type { Yoagent } from '../yoagent.d.ts'

/** Marks a stand-in agent, so `host-dialogs.ts` answers its questions. */
export const STAND_IN = Symbol.for('yoagent-rutis.dsh.stand-in')

/** The host's session service (yobuild's `host_session`). */
interface HostSession {
  info(): Promise<{ id: string; generation: number | string }>
  wake(text: string): Promise<unknown>
}

/** The host's workspace service (yobuild's `host_workspace`). */
interface HostWorkspace {
  cwd(): Promise<string>
}

/** The slices of DSH's services this uses. */
interface DshSession {
  readonly id: string
  readonly header: unknown
  readonly seq: number | bigint
  eventAt(seq: number): unknown
  append(type: string, payload: unknown): unknown
}
interface DshSessions {
  prepare(id?: string, options?: { meta?: { cwd?: string } }): DshSession
  enter(session: DshSession): () => void
  announce(session: DshSession): void
  flush(session: DshSession): Promise<boolean>
}
interface DshAgents {
  register(agent: unknown): unknown
}
interface DshPersistence {
  create(
    header: unknown,
  ): Promise<{ append(events: unknown[]): Promise<void>; [Symbol.asyncDispose]?(): Promise<void> }>
  flush?(): Promise<void>
}

/** What the tools adapter calls (the `yoagent-dsh-host` service). */
export interface DshHost {
  /** The stand-in for a call of run `runId`, its turn open. Throws when the host refuses one. */
  agentFor(runId: string): Promise<unknown>
  /** Run `runId` finished: its turn ends. */
  endRun(runId: string): Promise<void>
}

/** One generation's stand-in and its session. */
interface Generation {
  key: string
  agent: Record<string | symbol, unknown>
  session: DshSession
  turn: number
  /** The run whose turn is open, if any. */
  openFor?: string
  log?: { append(events: unknown[]): Promise<void>; flush?(): Promise<void>; [Symbol.asyncDispose]?(): Promise<void> }
  end: () => Promise<void>
}

const message = (error: unknown) => {
  try {
    return error instanceof Error ? error.message : String(error)
  } catch {
    return Object.prototype.toString.call(error)
  }
}

/** An error's message, with an `AggregateError`'s and a `cause`'s. */
const detail = (error: unknown): string => {
  const parts = [message(error)]
  const inner = (error as { errors?: unknown[] })?.errors
  if (Array.isArray(inner)) parts.push(...inner.map((e) => detail(e)))
  const cause = (error as { cause?: unknown })?.cause
  if (cause !== undefined) parts.push(`caused by ${detail(cause)}`)
  return parts.join('; ')
}

/** A DSH user message's text (its text blocks). */
const textOf = (message: unknown): string => {
  const content = (message as { content?: unknown })?.content
  if (typeof content === 'string') return content
  if (!Array.isArray(content)) return ''
  return content
    .map((block: { type?: string; text?: string }) => (block?.type === 'text' ? (block.text ?? '') : ''))
    .filter(Boolean)
    .join('\n')
}

// A plain Cordis plugin, as DSH's own are: DSH's scope needs a Cordis context.
export const name = 'yoagent-dsh-host'
export const inject = ['agents', 'sessions', 'sessionPersistence', 'host_session', 'host_workspace', 'yoagent']

// The Cordis context is used untyped: Cordis types are not needed at runtime.
export function apply(ctx: any) {
  const agents: DshAgents = ctx.agents
  const sessions: DshSessions = ctx.sessions
  const persistence: DshPersistence = ctx.sessionPersistence
  const hostSession: HostSession = ctx.host_session
  const hostWorkspace: HostWorkspace = ctx.host_workspace
  const yoagent: Yoagent = ctx.yoagent
  const say = (level: 'warn' | 'info' | 'debug', text: string) => {
    try {
      yoagent.log?.(level, text)?.catch(() => console.warn(text))
    } catch {
      console.warn(text)
    }
  }
  const reported = new Set<string>()
  const once = (key: string, level: 'warn' | 'info', text: string) => {
    if (reported.has(key)) return
    reported.add(key)
    say(level, text)
  }

  let current: Generation | undefined
  let closed = false
  // One operation at a time: parallel tool calls must not start two turns or two generations.
  let lock: Promise<unknown> = Promise.resolve()
  const serial = <T>(task: () => Promise<T>): Promise<T> => {
    const run = lock.then(task, task)
    lock = run.catch(() => {})
    return run
  }

  /** Steering from a DSH plugin, as text to the host. A refusal is reported, never dropped silently. */
  const wake = (via: string, msg: unknown) => {
    const text = textOf(msg)
    if (!text)
      return once(`empty-${via}`, 'info', `[dsh-host] a DSH plugin's ${via} carried no text; nothing sent to the host`)
    Promise.resolve()
      .then(() => hostSession.wake(text))
      .catch((error) => say('warn', `[dsh-host] the host refused a DSH plugin's ${via}: ${message(error)}`))
  }

  /**
   * The session's durability checkpoint. Once its log is open, the jsonl backend writes every
   * new session event itself (it routes `session/event` to the open handle); this drains and
   * flushes what it buffered.
   */
  const writeLog = async (g: Generation) => {
    if (!g.log) return
    try {
      await sessions.flush(g.session)
    } catch (error) {
      once(`log-${g.key}`, 'warn', `[dsh-host] writing the DSH session log failed: ${detail(error)}`)
    }
  }

  const closeTurn = async (g: Generation, kind: 'completed' | 'aborted') => {
    if (g.openFor === undefined) return
    g.openFor = undefined
    try {
      g.session.append('turn/end', { turn: g.turn, reason: { kind } })
    } catch (error) {
      say('warn', `[dsh-host] closing DSH turn ${g.turn} failed: ${message(error)}`)
    }
    await writeLog(g)
  }

  const start = async (key: string): Promise<Generation> => {
    const cwd = await hostWorkspace.cwd()
    if (typeof cwd !== 'string' || !cwd)
      throw new Error(`the host's workspace has no directory (${JSON.stringify(cwd)})`)
    const session = sessions.prepare(undefined, { meta: { cwd } })
    const detach = sessions.enter(session)
    sessions.announce(session)
    const noOp = (what: string) =>
      once(
        `noop-${what}`,
        'info',
        `[dsh-host] a DSH plugin asked the stand-in agent to ${what}; it runs no model, so nothing happens`,
      )
    const target: Record<string | symbol, unknown> = {
      [STAND_IN]: true,
      id: session.id,
      options: { provider: 'yoagent', model: 'yoagent' },
      session,
      // Empty and stays so: the host's agent takes the input (`wake`).
      inbox: {
        nextTurn: [],
        nextStep: [],
        clear() {},
        append() {},
        prepend() {},
        replace: () => false,
        remove: () => false,
        splice: () => [],
      },
      status: 'idle',
      ctx: undefined,
      cancel: () => noOp('cancel'),
      whenIdle: async () => {},
      runMaintenance: async (task: (signal: AbortSignal) => Promise<unknown>) => {
        noOp('run maintenance')
        return task(new AbortController().signal)
      },
      send: (msg: unknown, _target: unknown, wakeup: boolean) =>
        wakeup ? wake('send', msg) : noOp('queue a message without waking'),
      followup: (msg: unknown) => wake('follow-up', msg),
      steer: (msg: unknown) => wake('steering', msg),
      inject: (msg: unknown) => wake('injected context', msg),
    }
    const scope = createScope(ctx, target as never) as { ctx: unknown; dispose?: () => unknown }
    target.ctx = scope.ctx
    const unregister = agents.register(target) as unknown
    let log: Generation['log']
    try {
      log = await persistence.create(session.header)
      // Events from before the log was open; later ones reach it on their own.
      const before = Number(session.seq)
      if (before > 0) await log.append(Array.from({ length: before }, (_, i) => session.eventAt(i)))
    } catch (error) {
      once(
        'log-create',
        'warn',
        `[dsh-host] the DSH session log could not be opened (the session is not persisted): ${message(error)}`,
      )
    }
    const g: Generation = {
      key,
      agent: target,
      session,
      turn: 0,
      log,
      end: async () => {
        await closeTurn(g, 'aborted')
        await writeLog(g)
        try {
          await g.log?.[Symbol.asyncDispose]?.()
        } catch (error) {
          say('debug', `[dsh-host] closing the session log: ${message(error)}`)
        }
        for (const [what, dispose] of [
          ['unregister', unregister],
          ['scope', scope.dispose],
          ['session', detach],
        ] as const) {
          try {
            if (typeof dispose === 'function') await (dispose as () => unknown)()
          } catch (error) {
            say('debug', `[dsh-host] ending the stand-in (${what}): ${message(error)}`)
          }
        }
      },
    }
    say('debug', `[dsh-host] stand-in agent ${session.id} for host session ${key} in ${cwd}`)
    return g
  }

  /** The current generation's stand-in, a new one after a host reset. */
  const generation = async (): Promise<Generation> => {
    const info = await hostSession.info()
    const key = `${info?.id}#${info?.generation}`
    if (current?.key === key) return current
    const old = current
    current = undefined
    await old?.end()
    current = await start(key)
    return current
  }

  const host: DshHost = {
    agentFor: (runId) =>
      serial(async () => {
        if (closed) throw new Error('the DSH host is unloading')
        const g = await generation()
        if (g.openFor !== runId) {
          // Another run's turn still open: it ended without a `finish` (cancelled, lost).
          await closeTurn(g, 'aborted')
          g.turn++
          g.session.append('turn/start', { turn: g.turn })
          g.openFor = runId
        }
        return g.agent
      }),
    endRun: (runId) =>
      serial(async () => {
        if (current?.openFor === runId) await closeTurn(current, 'completed')
      }),
  }

  ctx.effect(() => ctx.provide('yoagent-dsh-host', host))
  // On unload: the open turn closed, the log written, the stand-in gone.
  ctx.effect(() => async () => {
    closed = true
    await serial(async () => {
      await current?.end()
      current = undefined
    })
  })
}
