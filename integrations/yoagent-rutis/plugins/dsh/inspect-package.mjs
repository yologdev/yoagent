// What a DSH plugin package asks of its host, read without starting it:
// the rows its bundle patch inserts (how DSH installs it; one row naming the
// package when it has none), the services those rows' plugins inject, the DSH
// packages it needs as peers, and whether it ships a web client UI.
// Prints JSON. Used by the ecosystem probe (examples/ecosystem_probe.rs).
//
//   node inspect-package.mjs <base dir> <package name>
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const [base, name] = process.argv.slice(2)
if (!base || !name) {
  console.error('usage: node inspect-package.mjs <base dir> <package name>')
  process.exit(2)
}
const require = createRequire(join(resolve(base), 'package.json'))
const manifestPath = require.resolve(`${name}/package.json`)
const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
const dsh = manifest.dsh ?? {}

const out = {
  version: manifest.version,
  peers: Object.keys(manifest.peerDependencies ?? {}).filter((p) => p.startsWith('@deepseek-ai/dsh-')),
  client: Boolean(dsh.client),
  rows: [{ id: 'target', name }],
  otherPatchOps: [],
  inject: null,
}

// The bundle patch: its `insert`s are the rows DSH adds when it installs the
// plugin. Anything else it does to the host's rows is listed, not applied.
if (dsh.bundle?.patch) {
  // `yaml` from beside this script (DSH's own dependencies carry it).
  const { parse } = createRequire(import.meta.url)('yaml')
  const patch = parse(readFileSync(join(dirname(manifestPath), dsh.bundle.patch), 'utf8')) ?? []
  const rows = []
  for (const op of patch) {
    if (Array.isArray(op.insert)) rows.push(...op.insert)
    for (const key of Object.keys(op)) if (key !== 'insert') out.otherPatchOps.push(key)
  }
  if (rows.length > 0) out.rows = rows
}

// What every row's plugin injects (a bundle's rows are other packages):
// what they wait for. Importing runs their module code.
const injectOf = async (spec) => {
  const mod = await import(pathToFileURL(require.resolve(spec)).href)
  const plugin = mod.default ?? mod
  const inject = plugin.inject ?? mod.inject
  return Array.isArray(inject)
    ? inject
    : inject && typeof inject === 'object'
      ? Object.entries(inject).map(([key, value]) => (value?.required === false ? `${key}?` : key))
      : []
}
const injected = new Set()
for (const row of out.rows) {
  if (typeof row.name !== 'string') continue
  try {
    for (const name of await injectOf(row.name)) injected.add(name)
  } catch (error) {
    // A row naming a package that is not there: the probe reports the row.
    if (row.name === name) out.importError = String(error?.message ?? error).slice(0, 500)
  }
}
out.inject = [...injected]
console.log(JSON.stringify(out))
