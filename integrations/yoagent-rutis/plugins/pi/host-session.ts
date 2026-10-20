// Load next to the pi extensions adapter, in the same Node runtime, when the
// host provides a `host_session` service and grants extensions `wakeFor`: their
// `pi.sendUserMessage`, and `pi.sendMessage` with `triggerTurn` or `deliverAs`,
// then reach the host as `wake(text, file)`.
//
// rutis shows a host service only to plugins that inject it, and starts a
// plugin only once everything it injects exists, so the adapter cannot
// declare it (hosts without one would never start it): this plugin injects
// it and re-provides it as `pi-host-session`, which the adapter looks up when
// an extension wakes the session. As with `host-ui.ts`, load it in one runtime only.

import { definePlugin } from '@arcships/rutis'

export default definePlugin({
  inject: ['host_session'],
  apply(ctx) {
    ctx.effect(ctx.provide('pi-host-session', ctx.use('host_session')))
  },
})
