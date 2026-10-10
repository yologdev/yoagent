// For the ecosystem probe (examples/ecosystem_probe.rs): which of the named
// Cordis services exist in this DSH runtime. A plugin injecting one that does
// not never starts, while its row still reads as started to the host, so
// the probe asks from inside. Writes `{ present, missing }` to `config.out`
// after `config.waitMs` (default 3000), giving late providers their chance.
//
// A plain Cordis plugin, injecting nothing, so it always starts.
import { writeFileSync } from 'node:fs'

export const name = 'yoagent-probe-services'

export interface Config {
  names: string[]
  out: string
  waitMs?: number
}

export function apply(ctx: any, config: Config) {
  const timer = setTimeout(() => {
    const present: string[] = []
    const missing: string[] = []
    for (const name of config.names ?? []) {
      let found = false
      try {
        found = ctx.get(name) !== undefined
      } catch {
        found = false
      }
      ;(found ? present : missing).push(name)
    }
    writeFileSync(config.out, JSON.stringify({ present, missing }))
  }, config.waitMs ?? 3000)
  ctx.effect(() => () => clearTimeout(timer))
}
