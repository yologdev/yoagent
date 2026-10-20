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
//                   the host may refuse (reported at warn)
//   host_workspace  cwd() — the workspace (an absolute path) the stand-in's
//                   session runs in
//
// Each host call is bounded (HOST_CALL_MS), and a tool call's own cancel
// stops its wait for the stand-in, queued behind other calls or not.
//
// One stand-in per host session generation: its id is its DSH session's id
// (a fresh `yoagent-<uuid>`: DSH's default ids restart at `session-1` in each
// process, which a session log already holding that id refuses), registered
// as a root with DSH's `agents`, its ctx a DSH scope (`createScope`), its
// session prepared, entered and announced in DSH's own `sessions` with
// `meta.cwd` the workspace. It never runs a model: its inbox stays empty,
// and what a plugin puts there, `cancel` and a non-waking `send` do nothing
// (reported at warn, once per kind and generation), `runMaintenance` runs its
// task at once (there is no turn to wait for);
// `steer` / `followup` / a waking `send` / `inject` go to
// `host_session.wake` as text.
//
// Turns belong to a top-level run: `turn/start` at its first DSH call, the
// runs it delegates to, at any depth, join it (`noteRun`, from the adapter's
// `tools` and `before_model` hooks: every run that carries the bridge, as its
// own extension or a parent's tree extension, tells the host who delegated
// it — a run without the bridge in between breaks the chain), `turn/end` at its `finish` —
// `completed`, or for a cancelled run `aborted` (user), a failed one `error`
// with the run's error. DSH's approvals need the open turn. One stand-in
// holds one turn at a time: a turn another top-level run finds still open
// (a lost `finish`, or two top-level runs at once) is closed `aborted` (hook)
// first, reported; the generation's end and unload close it `aborted`
// (disposed). The log is flushed after a turn ends without holding up
// `finish` (bounded by yoagent at 5 s).
//
// The session log: `sessionPersistence` (DSH's jsonl backend, from
// `dsh-host.json`) is opened once per generation; from then on the backend
// writes every session event itself, and each turn's end is a durability
// checkpoint (`sessions.flush`). The transcript stays empty: no yoagent
// conversation is mirrored into it.
//
// The tools adapter finds this through the `yoagent-dsh-host` service it
// provides (looked up per call, not injected), and `host-dialogs.ts` answers
// DSH's `approval/request` and the stand-in's questions through the host's
// `ui`.

import { randomUUID } from 'node:crypto'
import { isAbsolute } from 'node:path'
import { createScope } from '@deepseek-ai/dsh-scope'
import type { Yoagent } from '../yoagent.d.ts'

/** Marks a stand-in agent, so `host-dialogs.ts` answers its questions. */
export const STAND_IN = Symbol.for('yoagent-rutis.dsh.stand-in')

/** How long one call to a host service may take. */
const HOST_CALL_MS = 30_000

/** How long a call may wait for its stand-in, queue included (a generation may need several host calls). */
const WAIT_MS = 4 * HOST_CALL_MS

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
type DshLog = { append(events: unknown[]): Promise<void>; [Symbol.asyncDispose]?(): Promise<void> }
interface DshPersistence {
  create(header: unknown): Promise<DshLog>
}

/** How a yoagent run ended (the bridge's `Outcome.end`). */
export type RunEnd = 'completed' | 'stopped' | 'rejected' | 'cancelled' | 'failed' | 'other'

/** What the tools adapter calls (the `yoagent-dsh-host` service). */
export interface DshHost {
  /**
   * The stand-in for a call of run `runId` (a sub-agent's run names its parent), its turn open.
   * Throws when the host refuses one; `signal` stops the wait.
   */
  agentFor(runId: string, parentRunId: string | null, signal?: AbortSignal): Promise<unknown>
  /**
   * Run `runId` finished: the turn it owns ends — `completed`, `aborted` (user) for a cancelled
   * run, `error` with `message` for a failed one (DSH's turn ends have no "stopped" with a reason:
   * a stopped or rejected run ends `completed`).
   */
  endRun(runId: string, end: RunEnd, message?: string): Promise<void>
  /** Run `runId` started, delegated by `parentRunId` (null for a top-level run). */
  noteRun(runId: string, parentRunId: string | null): void
}

/** DSH's turn-end reasons this writes. */
type TurnEnd =
  | { kind: 'completed' }
  | { kind: 'aborted'; reason: { kind: 'user' } | { kind: 'disposed' } | { kind: 'hook'; reason: string } }
  | { kind: 'error'; error: { message: string; code: 'UNKNOWN' } }

/** One generation's stand-in and its session. */
interface Generation {
  key: string
  agent: Record<string | symbol, unknown>
  session: DshSession
  turn: number
  /** The top-level run whose turn is open, if any. */
  owner?: string
  log?: DshLog
  /** Torn down: a flush queued before is skipped. */
  ended?: boolean
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
const textOf = (msg: unknown): string => {
  const content = (msg as { content?: unknown })?.content
  if (typeof content === 'string') return content
  if (!Array.isArray(content)) return ''
  return content
    .map((block: { type?: string; text?: string }) => (block?.type === 'text' ? (block.text ?? '') : ''))
    .filter(Boolean)
    .join('\n')
}

/** `task`, unless it takes over `ms` or `signal` aborts first. */
const bounded = <T>(what: string, task: Promise<T>, ms: number, signal?: AbortSignal): Promise<T> =>
  new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`${what} did not answer within ${ms / 1000} s`)), ms)
    const aborted = () => reject(new Error(`${what}: the call was cancelled`))
    if (signal?.aborted) {
      clearTimeout(timer)
      return aborted()
    }
    signal?.addEventListener('abort', aborted, { once: true })
    task.then(resolve, reject).finally(() => {
      clearTimeout(timer)
      signal?.removeEventListener('abort', aborted)
    })
  })

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
  /** What the stand-in's queue is doing, for a call that gives up waiting. */
  let stage = 'idle'
  /** The top-level run each run belongs to (a sub-agent's run joins its root's turn). */
  const rootOf = new Map<string, string>()

  /** Steering from a DSH plugin, as text to the host. A refusal is reported. */
  const wake = (via: string, msg: unknown) => {
    const text = textOf(msg)
    if (!text) {
      say('warn', `[dsh-host] a DSH plugin's ${via} carried no text; nothing was sent to the host`)
      return
    }
    bounded(
      'host_session.wake',
      Promise.resolve().then(() => hostSession.wake(text)),
      HOST_CALL_MS,
    ).catch((error) => say('warn', `[dsh-host] the host refused a DSH plugin's ${via}: ${message(error)}`))
  }

  /** The session's durability checkpoint (the jsonl backend writes each event as it comes). */
  const writeLog = async (g: Generation) => {
    if (!g.log || g.ended) return
    try {
      const flushed = await bounded('writing the DSH session log', sessions.flush(g.session), HOST_CALL_MS)
      if (!flushed) once(`unflushed-${g.key}`, 'warn', `[dsh-host] no DSH session log took the session's events`)
    } catch (error) {
      once(`log-${g.key}`, 'warn', `[dsh-host] writing the DSH session log failed: ${detail(error)}`)
    }
  }

  const closeTurn = async (g: Generation, reason: TurnEnd, flush = true) => {
    if (g.owner === undefined) return
    try {
      g.session.append('turn/end', { turn: g.turn, reason })
    } catch (error) {
      say('warn', `[dsh-host] closing DSH turn ${g.turn} failed: ${message(error)}`)
    }
    g.owner = undefined
    if (flush) await writeLog(g)
  }

  /** The top-level run a run belongs to. */
  const rootFor = (runId: string, parentRunId: string | null) =>
    rootOf.get(runId) ?? (parentRunId ? (rootOf.get(parentRunId) ?? parentRunId) : runId)

  const start = async (key: string, signal?: AbortSignal): Promise<Generation> => {
    stage = 'asking the host for the workspace'
    const cwd = await bounded(
      'host_workspace.cwd',
      Promise.resolve().then(() => hostWorkspace.cwd()),
      HOST_CALL_MS,
      signal,
    )
    if (typeof cwd !== 'string' || !isAbsolute(cwd)) {
      throw new Error(`the host's workspace is not an absolute path (${JSON.stringify(cwd)})`)
    }
    const undo: [string, () => unknown][] = []
    const unwind = async () => {
      for (const [what, dispose] of undo.reverse()) {
        try {
          await bounded(`ending the stand-in (${what})`, Promise.resolve().then(dispose), HOST_CALL_MS)
        } catch (error) {
          say('warn', `[dsh-host] ending the stand-in (${what}) failed: ${message(error)}`)
        }
      }
      undo.length = 0
    }
    try {
      const session = sessions.prepare(`yoagent-${randomUUID()}`, { meta: { cwd } })
      undo.push(['session', sessions.enter(session)])
      sessions.announce(session)
      const ranMaintenance = () =>
        once(
          `maintenance-${key}`,
          'info',
          `[dsh-host] a DSH plugin's maintenance task ran at once (the stand-in has no turn to wait for)`,
        )
      const ignored = (what: string) =>
        once(
          `ignored-${key}-${what}`,
          'warn',
          `[dsh-host] a DSH plugin asked the stand-in agent to ${what}; it runs no model, so nothing happens`,
        )
      const target: Record<string | symbol, unknown> = {
        [STAND_IN]: true,
        id: session.id,
        options: { provider: 'yoagent', model: 'yoagent' },
        session,
        // Empty and stays so: the host's agent takes input (`wake`).
        inbox: {
          nextTurn: [],
          nextStep: [],
          clear() {},
          append: () => ignored('queue a message in its inbox'),
          prepend: () => ignored('queue a message in its inbox'),
          replace: () => false,
          remove: () => false,
          splice: () => {
            ignored('change its inbox')
            return []
          },
        },
        status: 'idle',
        ctx: undefined,
        cancel: () => ignored('cancel'),
        whenIdle: async () => {},
        runMaintenance: async (task: (signal: AbortSignal) => Promise<unknown>) => {
          ranMaintenance()
          return task(new AbortController().signal)
        },
        send: (msg: unknown, _target: unknown, wakeup: boolean) =>
          wakeup ? wake('send', msg) : ignored('queue a message without waking'),
        followup: (msg: unknown) => wake('follow-up', msg),
        steer: (msg: unknown) => wake('steering', msg),
        inject: (msg: unknown) => wake('injected context', msg),
      }
      const scope = createScope(ctx, target as never) as { ctx: unknown; dispose?: () => unknown }
      if (typeof scope.dispose === 'function') undo.push(['scope', () => scope.dispose!()])
      target.ctx = scope.ctx
      // DSH: await the registration (entry and announcement) before using the agent.
      const registration = agents.register(target) as (() => unknown) & PromiseLike<unknown>
      undo.push(['registration', () => registration()])
      stage = 'registering the stand-in with DSH'
      await bounded('registering the stand-in with DSH', Promise.resolve(registration), HOST_CALL_MS)
      stage = 'opening the DSH session log'
      let log: DshLog | undefined
      try {
        log = await bounded('opening the DSH session log', persistence.create(session.header), HOST_CALL_MS)
        undo.push(['session log', () => log?.[Symbol.asyncDispose]?.()])
      } catch (error) {
        once(
          `log-create-${key}`,
          'warn',
          `[dsh-host] the DSH session log could not be opened (this session is not persisted): ${detail(error)}`,
        )
      }
      // A fresh session has no events yet: the backend writes each one from here on.
      if (log && Number(session.seq) > 0) {
        try {
          await log.append(Array.from({ length: Number(session.seq) }, (_, i) => session.eventAt(i)))
        } catch (error) {
          once(
            `log-backfill-${key}`,
            'warn',
            `[dsh-host] the DSH session log is missing the session's first events: ${detail(error)}`,
          )
        }
      }
      const g: Generation = {
        key,
        agent: target,
        session,
        turn: 0,
        log,
        end: async () => {
          await closeTurn(g, { kind: 'aborted', reason: { kind: 'disposed' } })
          g.ended = true
          await unwind()
        },
      }
      say('debug', `[dsh-host] stand-in agent ${session.id} for host session ${key} in ${cwd}`)
      return g
    } catch (error) {
      // Started partway: undo what was, so a retry does not leave a session behind each time.
      await unwind()
      throw error
    }
  }

  /** The current generation's stand-in, a new one after a host reset. */
  const generation = async (signal?: AbortSignal): Promise<Generation> => {
    const info = await bounded(
      'host_session.info',
      Promise.resolve().then(() => hostSession.info()),
      HOST_CALL_MS,
      signal,
    )
    const id = info?.id
    const gen = info?.generation
    if (typeof id !== 'string' || !id || !(typeof gen === 'number' || (typeof gen === 'string' && gen))) {
      throw new Error(`the host's session info is not {id, generation}: ${JSON.stringify(info)}`)
    }
    const key = `${id}#${gen}`
    if (current?.key === key) return current
    const old = current
    current = undefined
    await old?.end()
    current = await start(key, signal)
    return current
  }

  const host: DshHost = {
    noteRun: (runId, parentRunId) => {
      rootOf.set(runId, rootFor(runId, parentRunId))
    },
    // A cancelled call stops waiting, also while queued behind other calls.
    agentFor: (runId, parentRunId, signal) =>
      bounded(
        'waiting for the DSH host',
        serial(async () => {
          if (closed) throw new Error('the DSH host is unloading')
          signal?.throwIfAborted()
          stage = 'asking the host which session this is'
          const g = await generation(signal)
          stage = 'idle'
          const root = rootFor(runId, parentRunId)
          rootOf.set(runId, root)
          if (g.owner === root) return g.agent
          if (g.owner !== undefined) {
            // The stand-in holds one turn: another top-level run's is still open (its `finish` was
            // lost, or the two runs overlap).
            say(
              'warn',
              `[dsh-host] top-level run ${root} needed the stand-in while run ${g.owner}'s DSH turn was open; that turn is closed as aborted (one turn at a time)`,
            )
            await closeTurn(g, {
              kind: 'aborted',
              reason: { kind: 'hook', reason: 'another top-level yoagent run needed the stand-in' },
            })
          }
          g.turn++
          g.session.append('turn/start', { turn: g.turn })
          g.owner = root
          return g.agent
        }),
        WAIT_MS,
        signal,
      ).catch((error) => {
        // Say what the host was doing when a call gave up on it.
        if (!signal?.aborted && stage !== 'idle') throw new Error(`${message(error)} (the DSH host was ${stage})`)
        throw error
      }),
    endRun: (runId, end, why) =>
      serial(async () => {
        const g = current
        rootOf.delete(runId)
        if (!g || g.owner !== runId) return
        await closeTurn(
          g,
          end === 'cancelled'
            ? { kind: 'aborted', reason: { kind: 'user' } }
            : end === 'failed'
              ? { kind: 'error', error: { message: why || 'the yoagent run failed', code: 'UNKNOWN' } }
              : { kind: 'completed' },
          false,
        )
        // The checkpoint after `finish` returns: yoagent gives `finish` 5 s, a flush may take longer.
        void serial(() => writeLog(g))
      }),
  }

  ctx.effect(() => ctx.provide('yoagent-dsh-host', host))
  // On unload: the open turn closed, the log flushed, the stand-in gone.
  ctx.effect(() => async () => {
    closed = true
    await serial(async () => {
      await current?.end()
      current = undefined
    })
  })
}
