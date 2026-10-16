// For the probe (`yoagent_rutis::probe::dsh`): which of the Cordis services
// a DSH plugin's rows inject exist in this DSH runtime. A plugin injecting
// one that does not never starts, while its row still reads as started to
// the host, so the probe asks from inside. What the rows inject is read
// from their modules here (`config.modules`, resolved from `config.base`'s
// package.json, as DSH would), plus any names in `config.names`. Writes
// `{ present, missing, importErrors }` to `config.out` after `config.waitMs`
// (default 3000), giving late providers their chance. A module that cannot
// be imported here is listed in `importErrors`: what it injects is unknown.
// Every injected service is checked: Cordis requires them all, even one
// declared `required: false`.
//
// A plain Cordis plugin, injecting nothing, so it always starts.
import { writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

export const name = 'yoagent-probe-services'

export interface Config {
  names?: string[]
  /** The plugin rows' module names (packages or files). */
  modules?: string[]
  /** The folder whose package.json the modules resolve from. */
  base?: string
  out: string
  waitMs?: number
}

/** What a module's plugin injects: `inject` as a list, or the keys of an object. Cordis treats every key as
 * required (`required: false` included): a plugin missing any of them never starts. */
async function injectOf(spec: string, base: string): Promise<string[]> {
  const require = createRequire(join(resolve(base), 'package.json'))
  const mod = await import(pathToFileURL(require.resolve(spec)).href)
  const plugin = mod.default ?? mod
  const inject = plugin.inject ?? mod.inject
  return Array.isArray(inject)
    ? inject
    : inject && typeof inject === 'object'
      ? Object.keys(inject)
      : []
}

export function apply(ctx: any, config: Config) {
  const names = new Set(config.names ?? [])
  const importErrors: Record<string, string> = {}
  const read = Promise.all(
    (config.modules ?? []).map(async (spec) => {
      try {
        for (const name of await injectOf(spec, config.base ?? process.cwd())) names.add(name)
      } catch (error: any) {
        importErrors[spec] = String(error?.message ?? error).slice(0, 300)
      }
    }),
  )
  const timer = setTimeout(async () => {
    await read
    const present: string[] = []
    const missing: string[] = []
    for (const entry of names) {
      let found = false
      try {
        found = ctx.get(entry) !== undefined
      } catch {
        found = false
      }
      ;(found ? present : missing).push(entry)
    }
    writeFileSync(config.out, JSON.stringify({ present, missing, importErrors }))
  }, config.waitMs ?? 3000)
  ctx.effect(() => () => clearTimeout(timer))
}
