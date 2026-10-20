// Load next to the pi extensions adapter, in the same Node runtime, when the
// host provides a `host_approval` service and grants an extension `exec:
// "gate"` (or `execFor[file] = "gate"`): each `pi.exec` call is then judged
// by the host (`judge(argv, cwd?)`, `ask(question, details | null, file)`).
//
// rutis shows a host service only to plugins that inject it, and starts a
// plugin only once everything it injects exists, so the adapter cannot
// declare it (hosts without one would never start it): this plugin injects
// it and re-provides it as `pi-host-approval`, which the adapter looks up
// when a gated call is made. As with `host-ui.ts`, load it in one runtime only.

import { definePlugin } from '@arcships/rutis'

export default definePlugin({
  inject: ['host_approval'],
  apply(ctx) {
    ctx.effect(ctx.provide('pi-host-approval', ctx.use('host_approval')))
  },
})
