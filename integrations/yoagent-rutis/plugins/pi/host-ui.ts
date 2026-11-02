// EXPERIMENTAL. Load next to the pi extensions adapter, in the same Node
// runtime, when the host provides a `ui` service (yoagent-frontend): pi
// extensions' dialogs (`ctx.ui.select`, `confirm`, `input`, `notify`) then
// reach the user instead of answering as pi's print mode.
//
// rutis shows a host service only to plugins that inject it, and starts a
// plugin only once everything it injects exists. So the adapter cannot
// inject `ui` (hosts without one would never start it): this plugin
// injects it and re-provides it as `pi-host-ui` (since rutis 0.12 that name
// reaches rutis too, so load this plugin in one runtime only), which the
// adapter declares `optional` and reads with `ctx.get` when it makes a pi
// context. Load it in the adapter's runtime: the adapter reads one this
// process has whenever it starts.

import { definePlugin } from '@arcships/rutis'

export default definePlugin({
  inject: ['ui'],
  apply(ctx) {
    ctx.effect(ctx.provide('pi-host-ui', ctx.use('ui')))
  },
})
