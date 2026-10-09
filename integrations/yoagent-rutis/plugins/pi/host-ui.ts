// EXPERIMENTAL. Load next to the pi extensions adapter, in the same Node
// runtime, when the host provides a `ui` service (yoagent-frontend): pi
// extensions' dialogs (`ctx.ui.select`, `confirm`, `input`, `notify`) then
// reach the user instead of answering as pi's print mode.
//
// rutis shows a host service only to plugins that inject it, and starts a
// plugin only once everything it injects exists. So the adapter cannot
// declare `ui` (hosts without one would never start it): this plugin
// injects it and re-provides it in the runtime as `pi-host-ui`, which the
// adapter looks up when it makes a pi context.

import { definePlugin } from '@arcships/rutis'

export default definePlugin({
  inject: ['ui'],
  apply(ctx) {
    ctx.effect(ctx.provide('pi-host-ui', ctx.use('ui')))
  },
})
