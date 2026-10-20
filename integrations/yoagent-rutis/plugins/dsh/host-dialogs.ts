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
//   approvals  with `dsh-host.ts` loaded, a call carries the stand-in agent,
//   (agent)    so a guard's `ask` goes through DSH's own approval service
//              (`@deepseek-ai/dsh-user-approval`, whose audit pair needs the
//              stand-in's open turn) and reaches `approval/request`: the
//              user is asked (a choice of "Allow once" / "Deny", with the
//              asker's reason and the call's arguments, kept from
//              `tools/pre-execute` by call id — a choice, not a confirm, so
//              no answer is told from a no: a host's confirm answers `false`
//              for both). Allow once → `allowed-once`, Deny → `rejected`, no
//              answer or the host failing → `unavailable`, the request
//              withdrawn → `cancelled`.
//   questions  `user-questions/request` from the stand-in agent is answered
//   (agent)    the same way as an agentless one (DSH refuses questions from
//              a child agent before they get here).
//
// With no frontend attached (`ui.frontends()` is 0) when a call starts, all
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

/** The choices of an approval for the stand-in's calls. */
const ALLOW_ONCE = 'Allow once'
const DENY = 'Deny'

/** Stand-in calls whose arguments are kept for an approval dialog that may follow. */
const MAX_PENDING_ARGS = 256

/** A question nobody answered: the tool reports it, rather than a made-up answer. */
class Unanswered extends Error {}

/** `dsh-host.ts` marks its stand-in agents with this. */
const STAND_IN = Symbol.for('yoagent-rutis.dsh.stand-in')
const isStandIn = (agent: unknown) => typeof agent === 'object' && agent !== null && (agent as Record<symbol, unknown>)[STAND_IN] === true

/** DSH's approval request (`@deepseek-ai/dsh-user-approval`), the fields read here. */
interface ApprovalRequest {
  agent?: unknown
  toolName: string
  callId?: string
  reason?: string
  displayReason?: { en?: string }
  signal?: AbortSignal
}
type ApprovalOutcome = 'allowed-once' | 'rejected' | 'cancelled' | 'unavailable'

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
   * (rutis's JavaScript runtime, as of 0.9), so an abort withdraws the question by its key: frontends
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

  /** A stand-in call's arguments, by call id, for its approval dialog (DSH's request carries none). */
  const pendingArgs = new Map<string, unknown>()
  const argsText = (args: unknown) => {
    try {
      return excerpt(JSON.stringify(args ?? {}, null, 1), MAX_ARGS_CHARS)
    } catch {
      return '(the arguments could not be shown)'
    }
  }

  // Outermost (prepended), so it sees what every dsh guard decided together.
  ctx.on(
    'tools/pre-execute',
    async (exec: PendingCall, next: () => Promise<PreToolDecision>): Promise<PreToolDecision> => {
      if (isStandIn(exec.agent)) {
        // DSH's own approval service asks for it after these listeners decide (below): keep the
        // arguments to show, the most recent MAX_PENDING_ARGS calls' (taken when the ask comes).
        pendingArgs.set(String(exec.callId), exec.arguments)
        if (pendingArgs.size > MAX_PENDING_ARGS) pendingArgs.delete(pendingArgs.keys().next().value!)
        return next()
      }
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

  // DSH's own approval service, for calls that carry the stand-in agent (`dsh-host.ts`).
  // Prepended, as the call policy above: this host's user answers first.
  ctx.on(
    'approval/request',
    async (request: ApprovalRequest, next: () => Promise<ApprovalOutcome>): Promise<ApprovalOutcome> => {
      if (!isStandIn(request.agent) || !attached()) return next()
      const reason = request.displayReason?.en ?? request.reason
      const id = request.callId === undefined ? undefined : String(request.callId)
      const args = id !== undefined && pendingArgs.has(id) ? `Arguments: ${argsText(pendingArgs.get(id))}` : undefined
      if (id !== undefined) pendingArgs.delete(id)
      try {
        const answer = await ask(
          {
            kind: 'select',
            title: `Allow the tool ${request.toolName}?`,
            message: [reason, args].filter(Boolean).join('\n\n'),
            options: [ALLOW_ONCE, DENY],
          },
          request.signal,
        )
        if (request.signal?.aborted) return 'cancelled'
        // No answer (dismissed, timed out, nobody attached) is not a no.
        return answer === ALLOW_ONCE ? 'allowed-once' : answer === DENY ? 'rejected' : 'unavailable'
      } catch (error) {
        if (request.signal?.aborted) return 'cancelled'
        say(`[dsh] asking the user to approve ${request.toolName} failed, so it is not approved: ${error}`)
        return 'unavailable'
      }
    },
    true,
  )

  // A request from another agent belongs to dsh's own (agent-scoped) answerers.
  ctx.on(
    'user-questions/request',
    async (request: AskUserQuestionRequestEvent, next: () => Promise<{ answers: AskUserQuestionAnswerItem[] }>) => {
      if ((request.agent !== undefined && !isStandIn(request.agent)) || !attached()) return next()
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
