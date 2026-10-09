// dsh's dialogs through the host's `ui` service (yoagent-frontend): load as
// a row in the dsh tools adapter's Node runtime, with the host's `ui` shared
// into it (`ui` in the loader's catalog; yoagent-frontend's `PluginHost`:
// `.share(services::UI)`). It injects `ui`, so it starts only on a host that
// provides one; elsewhere dsh stays as it is on its own (an `ask` is denied,
// `ask_user_question` finds no answerer).
//
// A plain Cordis plugin, as dsh's own are: it listens on dsh's events.
//
//   approval   a `tools/pre-execute` listener placed outermost, for the
//              adapter's calls only (no agent, `yoagent:` call ids): when
//              dsh's guards together decide `ask`, the user is asked
//              (confirm, with the arguments). Yes → allow. No, or no answer
//              in time → deny ("not approved"). The call cancelled →
//              cancel. The host failing → deny ("asking the user failed").
//              dsh's own approval service is not used: it needs an agent
//              and a dsh session, which these calls do not have.
//   questions  an answerer on `user-questions/request` for requests with no
//              agent — what `@deepseek-ai/dsh-tool-ask-user` sends from the
//              adapter's calls. Each question becomes a select (its distinct
//              option labels, plus "Other" — which then asks for typed text —
//              unless an option already has that label), a multiple select,
//              or a text input. A question left unanswered (dismissed, timed
//              out, no frontend) fails the request with that reason; a blank
//              input or an empty multiple choice is a skip (`selected: []`).
//
// With no frontend attached (`ui.frontends()` is 0) when a call starts, both
// step aside and dsh's own result stands.

import { randomUUID } from 'node:crypto'
import type { PreToolDecision } from '@deepseek-ai/dsh-tools'
import type { AskUserQuestionAnswerItem, AskUserQuestionRequestEvent } from '@deepseek-ai/dsh-user-questions/types'

export const name = 'yoagent-dsh-host-dialogs'
// `yoagent` (the bridge's service, always shared where the adapter runs) for
// the host's log.
export const inject = ['ui', 'yoagent']

/** The host's `ui` service (yoagent-frontend). */
interface HostUi {
  request(request: Record<string, unknown>, timeoutMs?: number): Promise<unknown>
  withdraw?(key: string): Promise<void>
  frontends?(): number
}

/** The fields of a pending call (dsh's `ToolExecution`) this plugin reads. */
interface PendingCall {
  callId: string
  name: string
  arguments?: unknown
  agent?: unknown
  signal?: AbortSignal
}

/** The choice that turns a select into typed text. */
const OTHER = 'Other (type an answer)'

/** Arguments shown in full up to this many characters; past it, head and tail. */
const MAX_ARGS_CHARS = 4000

/**
 * The text whole when it fits; else its head and tail around a note of the
 * length, so the end of a long command is never hidden. Cut by code points.
 */
const excerpt = (text: string, max: number) => {
  const chars = [...text]
  if (chars.length <= max) return text
  const head = Math.floor(max * 0.75)
  const tail = max - head
  return `${chars.slice(0, head).join('')}\n… (${chars.length} characters in all; the middle is not shown) …\n${chars.slice(-tail).join('')}`
}

/** A question nobody answered: the tool reports it, rather than a made-up answer. */
class Unanswered extends Error {}

// The Cordis context is used untyped: Cordis types are not needed at runtime.
export function apply(ctx: any) {
  const ui: HostUi = ctx.ui
  const say = (message: string) => {
    try {
      const log = ctx.yoagent?.log
      if (typeof log === 'function') return void Promise.resolve(log('warn', message)).catch(() => console.warn(message))
    } catch {}
    console.warn(message)
  }
  let frontendsFailed = false
  /** Whether a frontend is attached to answer (a host without `frontends`: assume so). */
  const attached = () => {
    if (typeof ui.frontends !== 'function') return true
    try {
      return Number(ui.frontends()) > 0
    } catch (error) {
      if (!frontendsFailed) say(`[dsh] the host's ui.frontends() failed, so dsh's dialogs stay closed: ${error}`)
      frontendsFailed = true
      return false
    }
  }

  /**
   * Ask through the host; `null` is no answer (dismissed, timed out, nobody
   * attached, withdrawn). The call to the host cannot itself be cancelled
   * (rutis 0.7), so an abort withdraws the question by its key: frontends
   * close it and the asker gets `null`.
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
      return (await ui.request({ ...request, key })) ?? null
    } finally {
      signal?.removeEventListener('abort', withdraw)
    }
  }

  // Outermost (prepended), so it sees what every dsh guard decided together.
  ctx.on(
    'tools/pre-execute',
    async (exec: PendingCall, next: () => Promise<PreToolDecision>): Promise<PreToolDecision> => {
      const decision = await next()
      const ours = exec.agent === undefined && String(exec.callId).startsWith('yoagent:')
      if (decision?.kind !== 'ask' || !ours || !attached()) return decision
      const reason = decision.displayReason?.en ?? decision.reason
      let args: string
      try {
        args = excerpt(JSON.stringify(exec.arguments ?? {}, null, 1), MAX_ARGS_CHARS)
      } catch {
        args = '(the arguments could not be shown)'
      }
      try {
        const answer = await ask(
          {
            kind: 'confirm',
            title: `Allow the tool ${exec.name}?`,
            message: [reason, `Arguments: ${args}`].filter(Boolean).join('\n\n'),
          },
          exec.signal,
        )
        if (exec.signal?.aborted) return { kind: 'cancel' }
        // The host's confirm answers `false` for a "no" and for no answer in time alike.
        return answer === true
          ? { kind: 'allow' }
          : { kind: 'deny', reason: `tool "${exec.name}" was not approved (declined, or no answer in time)` }
      } catch (error) {
        if (exec.signal?.aborted) return { kind: 'cancel' }
        say(`[dsh] asking the user to approve ${exec.name} failed, so it is denied: ${error}`)
        return { kind: 'deny', reason: `tool "${exec.name}" needs approval, and asking the user failed: ${error}` }
      }
    },
    true,
  )

  // A request with an agent belongs to dsh's own (agent-scoped) answerers.
  ctx.on(
    'user-questions/request',
    async (request: AskUserQuestionRequestEvent, next: () => Promise<{ answers: AskUserQuestionAnswerItem[] }>) => {
      if (request.agent !== undefined || !attached()) return next()
      const { signal } = request
      const answers: AskUserQuestionAnswerItem[] = []
      try {
        for (const question of request.questions) {
          if (signal?.aborted) break
          if (!attached()) throw new Unanswered(`no frontend is attached to answer question "${question.id}"`)
          const options = question.options ?? []
          // Each label once: an answer names options by label.
          const labels = [...new Set(options.map((option) => option.label))]
          const title = question.header ? `${question.header}: ${question.question}` : question.question
          const described = options.filter((option) => option.description).map((option) => `${option.label}: ${option.description}`)
          const message = [question.detail, ...described].filter(Boolean).join('\n')
          const unanswered = () => new Unanswered(`the user did not answer question "${question.id}" (no choice made, dismissed, timed out, or no frontend)`)
          const typed = async (): Promise<AskUserQuestionAnswerItem> => {
            const text = await ask({ kind: 'input', title, message }, signal)
            if (typeof text !== 'string') throw unanswered()
            // A blank submission is a skip.
            return text.trim() ? { id: question.id, selected: [], custom: text } : { id: question.id, selected: [] }
          }
          if (labels.length === 0) {
            answers.push(await typed())
          } else if (question.multiSelect) {
            const picked = await ask({ kind: 'select', title, message, options: labels, multiple: true }, signal)
            if (!Array.isArray(picked)) throw unanswered()
            answers.push({ id: question.id, selected: labels.filter((label) => picked.includes(label)) })
          } else {
            const other = labels.includes(OTHER) ? [] : [OTHER]
            const picked = await ask({ kind: 'select', title, message, options: [...labels, ...other] }, signal)
            if (typeof picked !== 'string' || !(labels.includes(picked) || other.includes(picked))) throw unanswered()
            answers.push(other.includes(picked) ? await typed() : { id: question.id, selected: [picked] })
          }
        }
      } catch (error) {
        if (signal?.aborted || error instanceof Unanswered) throw error
        say(`[dsh] asking the user a question failed: ${error}`)
        throw new Error(`asking the user failed: ${error}`)
      }
      // dsh's service turns a request aborted meanwhile into its own ASK_ABORTED.
      if (signal?.aborted) throw new Error('the question was withdrawn')
      return { answers }
    },
  )
}
