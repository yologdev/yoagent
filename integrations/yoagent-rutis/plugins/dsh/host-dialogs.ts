// dsh's dialogs through the host's `ui` service (yoagent-frontend): load as
// a row in the dsh tools adapter's Node runtime. It injects `ui`, so it
// starts only on a host that provides one; elsewhere dsh stays as it is on
// its own (an `ask` is denied, `ask_user_question` finds no answerer).
//
// A plain Cordis plugin, as dsh's own are: it listens on dsh's events.
//
//   approval   a `tools/pre-execute` listener placed outermost, for the
//              adapter's calls only (no agent, `yoagent:` call ids): when
//              dsh's guards together decide `ask`, the user is asked
//              (confirm). Yes → allow, no → deny, the call cancelled →
//              cancel. dsh's own approval service is not used: it needs an
//              agent and a dsh session, which these calls do not have.
//   questions  an answerer on `user-questions/request` for requests with no
//              agent — what `@deepseek-ai/dsh-tool-ask-user` sends from the
//              adapter's calls. Each question becomes a select (its options,
//              plus "Other" for typed text), a multiple select, or an input.
//
// With no frontend attached (`ui.frontends()` is 0), both step aside.

import { randomUUID } from 'node:crypto'

export const name = 'yoagent-dsh-host-dialogs'
export const inject = ['ui']

/** The host's `ui` service (yoagent-frontend). */
interface HostUi {
  request(request: Record<string, unknown>, timeoutMs?: number): Promise<unknown>
  withdraw?(key: string): Promise<void>
  frontends?(): number
}

/** dsh's pre-execute decision (`PreToolDecision`). */
type Decision =
  | { kind: 'allow' }
  | { kind: 'deny'; reason: string }
  | { kind: 'cancel' }
  | { kind: 'ask'; reason?: string; displayReason?: { en: string; [locale: string]: string } }

/** A pending call as `tools/pre-execute` sees it. */
interface PendingCall {
  callId: string
  name: string
  arguments?: unknown
  agent?: unknown
  signal?: AbortSignal
}

/** A `user-questions/request` (`@deepseek-ai/dsh-user-questions`). */
interface QuestionRequest {
  questions: {
    id: string
    question: string
    detail?: string
    header?: string
    options?: { label: string; description?: string }[]
    multiSelect?: boolean
  }[]
  agent?: unknown
  signal?: AbortSignal
}

type Answer = { id: string; selected: string[]; custom?: string }

/** The choice that turns a select into typed text. */
const OTHER = 'Other (type an answer)'

/** Cut to `max` characters (code points: never half a surrogate pair). */
const cut = (text: string, max: number) => {
  const chars = [...text]
  return chars.length > max ? `${chars.slice(0, max - 1).join('')}…` : text
}

// The Cordis context is used untyped: Cordis types are not needed at runtime.
export function apply(ctx: any) {
  const ui: HostUi = ctx.ui
  const say = (message: string) => {
    // The host's log when the bridge's `yoagent` service is here; else stderr.
    try {
      const log = ctx.get('yoagent')?.log
      if (typeof log === 'function') return void Promise.resolve(log('warn', message)).catch(() => console.warn(message))
    } catch {}
    console.warn(message)
  }
  /** Whether a frontend is attached to answer (a host without `frontends`: assume so). */
  const attached = () => {
    try {
      return typeof ui.frontends !== 'function' || Number(ui.frontends()) > 0
    } catch {
      return false
    }
  }

  /**
   * Ask through the host. The call to the host cannot itself be cancelled
   * (rutis 0.7), so an abort withdraws the question by its key: frontends
   * close it and the asker gets the safe default.
   */
  const ask = async (request: Record<string, unknown>, signal?: AbortSignal): Promise<unknown> => {
    if (signal?.aborted) return null
    const key = `dsh-${randomUUID()}`
    const withdraw = () => {
      if (typeof ui.withdraw !== 'function') return
      const failed = (error: unknown) => say(`[dsh] could not withdraw a question (it stays open until its timeout): ${error}`)
      try {
        Promise.resolve(ui.withdraw(key)).catch(failed)
      } catch (error) {
        failed(error)
      }
    }
    signal?.addEventListener('abort', withdraw, { once: true })
    try {
      return await ui.request({ ...request, key })
    } finally {
      signal?.removeEventListener('abort', withdraw)
    }
  }

  // Outermost (prepended), so it sees what every dsh guard decided together.
  ctx.on(
    'tools/pre-execute',
    async (exec: PendingCall, next: () => Promise<Decision>): Promise<Decision> => {
      const decision = await next()
      const ours = exec.agent === undefined && String(exec.callId).startsWith('yoagent:')
      if (decision?.kind !== 'ask' || !ours || !attached()) return decision
      const reason = decision.displayReason?.en ?? decision.reason
      let args = ''
      try {
        args = cut(JSON.stringify(exec.arguments ?? {}), 300)
      } catch {}
      try {
        const answer = await ask(
          {
            kind: 'confirm',
            title: `Allow the tool ${exec.name}?`,
            message: [reason, args && `Arguments: ${args}`].filter(Boolean).join('\n\n'),
          },
          exec.signal,
        )
        if (exec.signal?.aborted) return { kind: 'cancel' }
        return answer === true ? { kind: 'allow' } : { kind: 'deny', reason: `the user did not allow tool "${exec.name}"` }
      } catch (error) {
        if (exec.signal?.aborted) return { kind: 'cancel' }
        say(`[dsh] asking the user to approve ${exec.name} failed, so it is denied: ${error}`)
        return decision
      }
    },
    true,
  )

  // A request with an agent belongs to dsh's own (agent-scoped) answerers.
  ctx.on('user-questions/request', async (request: QuestionRequest, next: () => Promise<{ answers: Answer[] }>) => {
    if (request.agent !== undefined || !attached()) return next()
    const { signal } = request
    const answers: Answer[] = []
    try {
      for (const question of request.questions) {
        if (signal?.aborted) break
        const options = question.options ?? []
        // Each label once: an answer names options by label.
        const labels = [...new Set(options.map((option) => option.label))]
        const title = question.header ? `${question.header}: ${question.question}` : question.question
        const described = options.filter((option) => option.description).map((option) => `${option.label}: ${option.description}`)
        const message = [question.detail, ...described].filter(Boolean).join('\n')
        const typed = async (): Promise<Answer> => {
          const text = await ask({ kind: 'input', title, message }, signal)
          return typeof text === 'string' && text.trim()
            ? { id: question.id, selected: [], custom: text }
            : { id: question.id, selected: [] }
        }
        if (labels.length === 0) {
          answers.push(await typed())
        } else if (question.multiSelect) {
          const picked = await ask({ kind: 'select', title, message, options: labels, multiple: true }, signal)
          answers.push({ id: question.id, selected: Array.isArray(picked) ? labels.filter((label) => picked.includes(label)) : [] })
        } else {
          const other = labels.includes(OTHER) ? [] : [OTHER]
          const picked = await ask({ kind: 'select', title, message, options: [...labels, ...other] }, signal)
          if (picked === OTHER && other.length > 0) answers.push(await typed())
          else answers.push({ id: question.id, selected: typeof picked === 'string' && labels.includes(picked) ? [picked] : [] })
        }
      }
    } catch (error) {
      if (signal?.aborted) throw error
      say(`[dsh] asking the user a question failed: ${error}`)
      return next()
    }
    // dsh's service turns a request aborted meanwhile into its own ASK_ABORTED.
    if (signal?.aborted) throw new Error('the question was withdrawn')
    return { answers }
  })
}
