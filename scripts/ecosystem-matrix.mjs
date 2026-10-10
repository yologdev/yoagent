#!/usr/bin/env node
// The plugin compatibility matrix: the most-downloaded pi packages and DSH
// plugins on npm, each installed unchanged into its own scratch directory
// and loaded through yoagent-rutis's adapters by the ecosystem probe
// (integrations/yoagent-rutis/examples/ecosystem_probe.rs). Writes a
// Markdown report and the raw results.
//
//   node scripts/ecosystem-matrix.mjs [--top 20] [--only pi|dsh]
//        [--packages name,name] [--out docs/evals/plugin-compatibility.md]
//        [--json PATH] [--work DIR]
//   node scripts/ecosystem-matrix.mjs --from-json RESULTS.json [--out ...]
//        (the report again from earlier raw results: no installs, no probe)
//
// Needs Node 24+, npm, cargo, and `npm ci` in the adapters' directories
// (scripts/try-coding-agent.sh --setup-only does it). Network: npm only.
// Installs run with --ignore-scripts; loading a package still runs its code,
// as its own host would, so run this where that is acceptable (CI, a VM).
import { spawnSync } from 'node:child_process'
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const rutis = join(repo, 'integrations/yoagent-rutis')
const piDir = join(rutis, 'plugins/pi')
const dshDir = join(rutis, 'plugins/dsh')

const opts = { top: 20, only: null, packages: null, fromJson: null, out: join(repo, 'docs/evals/plugin-compatibility.md'), json: null, work: null }
const argv = process.argv.slice(2)
for (let i = 0; i < argv.length; i++) {
  const next = () => argv[++i]
  switch (argv[i]) {
    case '--top': opts.top = Number(next()); break
    case '--only': opts.only = next(); break
    case '--packages': opts.packages = next().split(','); break
    case '--out': opts.out = resolve(next()); break
    case '--json': opts.json = resolve(next()); break
    case '--work': opts.work = resolve(next()); break
    case '--from-json': opts.fromJson = resolve(next()); break
    default: console.error(`unknown argument ${argv[i]}`); process.exit(2)
  }
}
opts.json ??= opts.out.replace(/\.md$/, '.json')
const work = opts.work ?? mkdtempSync(join(tmpdir(), 'yoagent-matrix-'))
mkdirSync(work, { recursive: true })
const log = (...a) => console.error(...a)

function run(cmd, args, { cwd, timeout = 300_000 } = {}) {
  const r = spawnSync(cmd, args, { cwd, timeout, encoding: 'utf8', maxBuffer: 64 << 20 })
  return { ok: r.status === 0, stdout: r.stdout ?? '', stderr: r.stderr ?? '', timedOut: r.error?.code === 'ETIMEDOUT' }
}
const tail = (text, n = 600) => text.trim().split('\n').filter((l) => !/ExperimentalWarning|trace-warnings/.test(l)).join('\n').slice(-n)

// The top packages by monthly downloads among npm's search results for a
// keyword (the search API returns at most 1000; ranked by its own score,
// then re-ranked here by downloads).
async function top(keyword) {
  const seen = new Map()
  for (let from = 0; from < 1000; from += 250) {
    const url = `https://registry.npmjs.org/-/v1/search?text=keywords:${keyword}&size=250&from=${from}`
    const res = await fetch(url)
    if (!res.ok) throw new Error(`${url}: ${res.status}`)
    const { objects, total } = await res.json()
    for (const o of objects) seen.set(o.package.name, { name: o.package.name, version: o.package.version, monthly: o.downloads?.monthly ?? 0, description: o.package.description ?? '' })
    if (from + 250 >= total) break
  }
  const all = [...seen.values()].sort((a, b) => b.monthly - a.monthly)
  if (opts.packages) return opts.packages.map((n) => all.find((p) => p.name === n) ?? { name: n, version: 'latest', monthly: 0, description: '' })
  return all.slice(0, opts.top)
}

// The probe binary, built once (not for --from-json).
let probe
if (!opts.fromJson) {
log('building the probe…')
const build = run('cargo', ['build', '--quiet', '--manifest-path', join(rutis, 'Cargo.toml'), '--features', 'node', '--example', 'ecosystem_probe', '--message-format=json'], { timeout: 1_800_000 })
if (!build.ok) throw new Error(`cargo build failed:\n${tail(build.stderr, 4000)}`)
probe = build.stdout.split('\n').filter(Boolean).map((l) => { try { return JSON.parse(l) } catch { return {} } })
  .find((m) => m.reason === 'compiler-artifact' && m.target?.name === 'ecosystem_probe')?.executable
if (!probe) throw new Error('the probe binary was not found in cargo output')
}

function probeJson(args) {
  const r = run(probe, args, { timeout: 180_000 })
  try {
    return { ...JSON.parse(r.stdout.trim().split('\n').pop()), stderr: tail(r.stderr) }
  } catch {
    return { error: r.timedOut ? 'the probe timed out (180 s)' : `the probe failed: ${tail(r.stderr)}` }
  }
}

const npmInstall = (dir, spec, extra) =>
  run('npm', ['install', spec, '--ignore-scripts', '--no-audit', '--no-fund', '--loglevel=error', ...extra], { cwd: dir, timeout: 300_000 })

// ---- pi ------------------------------------------------------------------

function classifyPi(r) {
  const warnings = (r.logs ?? []).filter((l) => l.level === 'warn').map((l) => l.message)
  const ignored = warnings.flatMap((w) => (w.match(/ignored: (.*)$/)?.[1] ?? '').split('; ').filter(Boolean)).map((s) => s.replace(/^[^:]+: /, ''))
  const skills = r.skills?.loaded ?? []
  if (r.install) return { result: 'not installed', detail: r.install }
  if ((r.resources?.extensions ?? 0) === 0) {
    if (skills.length > 0) return { result: 'works', detail: `skills only: ${skills.length} loaded by yoagent's skills loader` }
    const what = Object.entries(r.resources ?? {}).filter(([, n]) => n > 0).map(([k, n]) => `${n} ${k}`).join(', ')
    return { result: 'not applicable', detail: `no extensions or skills${what ? ` (${what}: pi's app features)` : ''}` }
  }
  if (r.loaded) {
    const parts = []
    if (r.tools.length > 0) parts.push(`${r.tools.length} tool${r.tools.length === 1 ? '' : 's'}`)
    const events = (r.logs ?? []).flatMap((l) => l.message.match(/events ([^;]*);/)?.[1].split(', ') ?? []).filter((e) => e !== 'none')
    if (events.includes('tool_call') || events.includes('tool_result')) parts.push('tool-call hooks')
    if (events.includes('input')) parts.push('input hooks')
    if (r.note_chars > 0) parts.push('prompt additions')
    if (skills.length > 0) parts.push(`${skills.length} skills`)
    // Loaded, but nothing an agent run would use: often a package that
    // registers its features only once configured (keys, a config file).
    if (parts.length === 0) {
      return {
        result: 'nothing to use',
        detail: ignored.length > 0
          ? 'loads; everything it registers is ignored here'
          : `loads but registers ${events.length > 0 ? `only ${events.join(', ')}` : 'nothing'} as installed (it may need configuration first)`,
        ignored,
      }
    }
    if ((r.without ?? []).length > 0) parts.push(`replaces yoagent's ${r.without.join(', ')}`)
    return {
      result: ignored.length > 0 ? 'works, partly' : 'works',
      detail: parts.join(', '),
      ignored,
    }
  }
  const error = r.error ?? 'unknown'
  if (/never fires/.test(error)) return { result: 'refused', detail: error.replace(/^.*?Error: /s, '') }
  // A throwing session_start refuses the load too (fail closed), typically
  // reaching for pi app internals the adapter's context does not have.
  if (/failed in session_start/.test(error)) return { result: 'refused', detail: `its session_start failed, so the adapter did not load it: ${error.replace(/^.*?session_start: /s, '')}` }
  return { result: 'failed', detail: error.replace(/^.*?Error: /s, '').slice(0, 400) }
}

async function runPi(packages) {
  const results = []
  for (const [i, pkg] of packages.entries()) {
    log(`pi ${i + 1}/${packages.length}: ${pkg.name}@${pkg.version}`)
    const dir = join(work, 'pi', pkg.name.replace('/', '__'))
    rmSync(dir, { recursive: true, force: true })
    mkdirSync(dir, { recursive: true })
    writeFileSync(join(dir, 'package.json'), '{"private":true}')
    // pi supplies its own packages to extensions (peers): never installed here.
    const inst = npmInstall(dir, `${pkg.name}@${pkg.version}`, ['--legacy-peer-deps'])
    let r
    if (!inst.ok) {
      r = { install: tail(inst.stderr, 300) || 'npm install failed' }
    } else {
      const res = run('node', [join(piDir, 'resolve-package.mjs'), join(dir, 'node_modules', pkg.name)], { cwd: piDir })
      const found = res.ok ? JSON.parse(res.stdout) : { extensions: [], skills: [], prompts: [], themes: [] }
      const cwd = join(dir, 'project')
      mkdirSync(cwd, { recursive: true })
      r = probeJson(['pi', '--cwd', cwd, ...found.skills.flatMap((s) => ['--skill', s]), ...found.extensions])
      r.resources = Object.fromEntries(Object.entries(found).map(([k, v]) => [k, v.length]))
    }
    results.push({ ...pkg, ...r, ...classifyPi(r) })
    log(`  → ${results.at(-1).result}: ${results.at(-1).detail}`)
  }
  return results
}

// ---- DSH -----------------------------------------------------------------

function classifyDsh(r) {
  if (r.install) return { result: 'not installed', detail: r.install }
  if (r.inspectError) return { result: 'failed', detail: r.inspectError }
  const missing = r.services?.missing ?? []
  const failed = (r.rows ?? []).filter((row) => row.state !== 'active')
  const ui = r.client ? ' Its web UI needs DSH\'s web client.' : ''
  if (r.error) return { result: 'failed', detail: r.error.slice(0, 400) }
  if (missing.length > 0) return { result: 'needs a DSH host', detail: `waits for services yoagent's DSH runtime does not run: ${missing.join(', ')}.${ui}` }
  const dshPackages = [...new Set((r.rows ?? []).flatMap((row) =>
    [...(row.error ?? '').matchAll(/(?:no plugin named "|Cannot find package ')(@deepseek-ai\/[^"']+)/g)].map((m) => m[1])))]
  if (dshPackages.length > 0) return { result: 'needs a DSH host', detail: `loads DSH packages an agent library does not ship: ${dshPackages.join(', ')}.${ui}` }
  if (failed.length > 0) return { result: 'failed', detail: failed.map((row) => `${row.id}: ${row.state} ${row.error ?? ''}`).join('; ').slice(0, 400) }
  const parts = []
  if ((r.tools ?? []).length > 0) parts.push(`${r.tools.length} tool${r.tools.length === 1 ? '' : 's'}`)
  if (r.guidance_chars > 0) parts.push('prompt guidance')
  if (parts.length > 0) return { result: 'works', detail: parts.join(', ') + '.' + ui }
  return { result: 'not applicable', detail: `starts, but adds no tools or guidance (UI, settings or server features).${ui}` }
}

// Each package of `from` (a node_modules) copied into `to`, unless `to`
// has it: the host's DSH and rutis packages are kept; any other package the
// base already has at another version goes under the plugin's own
// node_modules, where only the plugin resolves it.
function mergeInto(from, to, plugin) {
  const names = readdirSync(from).filter((n) => !n.startsWith('.'))
    .flatMap((n) => (n.startsWith('@') ? readdirSync(join(from, n)).map((m) => `${n}/${m}`) : [n]))
  // The plugin first: nesting a conflicting package creates its directory.
  cpSync(join(from, plugin), join(to, plugin), { recursive: true, verbatimSymlinks: true })
  for (const name of names.filter((n) => n !== plugin)) {
    const target = join(to, name)
    if (!existsSync(target)) {
      cpSync(join(from, name), target, { recursive: true, verbatimSymlinks: true })
    } else if (!/^@(deepseek-ai|arcships)\//.test(name)) {
      const nested = join(to, plugin, 'node_modules', name)
      if (!existsSync(nested)) cpSync(join(from, name), nested, { recursive: true, verbatimSymlinks: true })
    }
  }
}

// The DSH release the base pins (all of DSH's packages share it).
const dshRelease = JSON.parse(readFileSync(join(dshDir, 'package.json'), 'utf8')).dependencies['@deepseek-ai/dsh-tools']

async function runDsh(packages) {
  // A base install (the adapter's package.json and lock) made once, then
  // copied per package, so one package's dependencies never reach another.
  const base = join(work, 'dsh-base')
  if (!existsSync(join(base, 'node_modules/@arcships/rutis-runtime'))) {
    log('preparing the DSH base…')
    rmSync(base, { recursive: true, force: true })
    mkdirSync(base, { recursive: true })
    for (const f of ['package.json', 'package-lock.json']) cpSync(join(dshDir, f), join(base, f))
    const ci = run('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund', '--loglevel=error'], { cwd: base, timeout: 600_000 })
    if (!ci.ok) throw new Error(`npm ci for the DSH base failed:\n${tail(ci.stderr, 2000)}`)
  }
  const results = []
  for (const [i, pkg] of packages.entries()) {
    log(`dsh ${i + 1}/${packages.length}: ${pkg.name}@${pkg.version}`)
    const dir = join(work, 'dsh', pkg.name.replace('/', '__'))
    rmSync(dir, { recursive: true, force: true })
    cpSync(base, dir, { recursive: true, verbatimSymlinks: true })
    // The adapter and the service check resolve this directory's Cordis.
    for (const f of ['dsh-tools-adapter.ts', 'probe-services.ts']) cpSync(join(dshDir, f), join(dir, f))
    // The plugin is installed alone, without peers, then merged into the
    // base copy without npm: npm would re-plan the base's tree (swapping or
    // pruning DSH's packages). A DSH host supplies DSH's packages, as pi
    // supplies its own, so the base's win; a peer the base lacks shows as a
    // failed import, an unknown row, or a missing service.
    //
    // A DSH host has DSH's whole release, so the plugin's DSH peer packages
    // the base lacks come along at the base's release (libraries it imports);
    // what still gates it are the DSH services it injects and the DSH plugins
    // its patch loads, which the probe reports.
    const alone = join(work, 'dsh-alone', pkg.name.replace('/', '__'))
    rmSync(alone, { recursive: true, force: true })
    mkdirSync(alone, { recursive: true })
    writeFileSync(join(alone, 'package.json'), '{"private":true}')
    const view = run('npm', ['view', `${pkg.name}@${pkg.version}`, 'peerDependencies', '--json'], { cwd: alone, timeout: 60_000 })
    let peers = {}
    try { peers = JSON.parse(view.stdout || '{}') ?? {} } catch {}
    const extra = Object.keys(peers)
      .filter((p) => p.startsWith('@deepseek-ai/') && !existsSync(join(dir, 'node_modules', p)))
      .map((p) => (p.startsWith('@deepseek-ai/dsh-') ? `${p}@${dshRelease}` : `${p}@${peers[p]}`))
    const spec = `${pkg.name}@${pkg.version}`
    let inst = npmInstall(alone, spec, [...extra])
    if (!inst.ok) inst = npmInstall(alone, spec, [...extra, '--legacy-peer-deps'])
    if (!inst.ok && extra.length > 0) inst = npmInstall(alone, spec, ['--legacy-peer-deps'])
    if (inst.ok) mergeInto(join(alone, 'node_modules'), join(dir, 'node_modules'), pkg.name)
    rmSync(alone, { recursive: true, force: true })
    let r
    if (!inst.ok) {
      r = { install: tail(inst.stderr, 300) || 'npm install failed' }
    } else {
      const ins = run('node', [join(dshDir, 'inspect-package.mjs'), dir, pkg.name], { cwd: dir })
      let info
      try { info = JSON.parse(ins.stdout.trim().split('\n').pop()) } catch { info = null }
      if (!info) {
        r = { inspectError: `could not read the package: ${tail(ins.stderr, 300)}` }
      } else {
        const inject = (info.inject ?? []).filter((n) => !n.endsWith('?'))
        r = { ...probeJson(['dsh', '--base', dir, '--rows', JSON.stringify(info.rows), ...inject.flatMap((n) => ['--inject', n])]), client: info.client, peers: info.peers, inject: info.inject, importError: info.importError }
      }
    }
    results.push({ ...pkg, ...r, ...classifyDsh(r) })
    log(`  → ${results.at(-1).result}: ${results.at(-1).detail}`)
    rmSync(dir, { recursive: true, force: true })
  }
  return results
}

// ---- report --------------------------------------------------------------

const pinned = (dir, name) => JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8')).dependencies?.[name]
const esc = (s) => String(s ?? '').replace(/\|/g, '\\|').replace(/\n/g, ' ')
const count = (rows) => Object.entries(rows.reduce((acc, r) => ((acc[r.result] = (acc[r.result] ?? 0) + 1), acc), {})).map(([k, n]) => `${n} ${k}`).join(' · ')

function table(rows) {
  const lines = ['| # | Package | Downloads / month | Result | What yoagent gets |', '|---|---|---:|---|---|']
  rows.forEach((r, i) => {
    const link = `[${esc(r.name)}](https://www.npmjs.com/package/${r.name})`
    let what = esc(r.detail)
    if (r.ignored?.length) what += `<br><sub>ignored: ${esc(r.ignored.join('; ')).slice(0, 300)}</sub>`
    lines.push(`| ${i + 1} | ${link} ${esc(r.version)} | ${r.monthly.toLocaleString('en-US')} | ${r.result} | ${what} |`)
  })
  return lines.join('\n')
}

let raw
if (opts.fromJson) {
  // Classified again with this script's rules; the probe results are kept.
  raw = JSON.parse(readFileSync(opts.fromJson, 'utf8'))
  const again = (rows, classify) => rows?.map(({ result, detail, ignored, ...r }) => ({ ...r, ...classify(r) }))
  raw.pi = again(raw.pi, classifyPi)
  raw.dsh = again(raw.dsh, classifyDsh)
} else {
  // With --only, the other ecosystem's earlier results are kept.
  const earlier = opts.only && existsSync(opts.json) ? JSON.parse(readFileSync(opts.json, 'utf8')) : {}
  raw = { ...earlier, date: new Date().toISOString().slice(0, 10), top: opts.top }
  if (opts.only !== 'dsh') raw.pi = await runPi(await top('pi-package'))
  if (opts.only !== 'pi') raw.dsh = await runDsh(await top('dsh-plugin'))
}
const date = raw.date
const sections = []
mkdirSync(dirname(opts.json), { recursive: true })
writeFileSync(opts.json, JSON.stringify(raw, null, 2) + '\n')

if (raw.pi) {
  sections.push(`## pi packages

The ${raw.pi.length} most-downloaded packages with the \`pi-package\` keyword, through the pi adapter (pi ${pinned(piDir, '@earendil-works/pi-coding-agent')}). ${count(raw.pi)}.

${table(raw.pi)}`)
}
if (raw.dsh) {
  sections.push(`## DSH plugins

The ${raw.dsh.length} most-downloaded packages with the \`dsh-plugin\` keyword, over DSH's base packages and the tools adapter (DSH ${pinned(dshDir, '@deepseek-ai/dsh-tools')}). ${count(raw.dsh)}.

${table(raw.dsh)}`)
}

// The headline, computed from the rows.
const n = (rows, ...results) => rows.filter((r) => results.includes(r.result)).length
const summary = []
if (raw.pi) {
  summary.push(`- **pi:** ${n(raw.pi, 'works', 'works, partly')} of ${raw.pi.length} give an agent run tools, tool-call or input hooks, or prompt additions (${n(raw.pi, 'works')} fully). ${n(raw.pi, 'refused')} are refused because they reach into what yoagent cannot enforce (provider requests, the context, pi's model registry); ${n(raw.pi, 'nothing to use')} register nothing usable as installed.`)
}
if (raw.dsh) {
  summary.push(`- **DSH:** ${n(raw.dsh, 'works')} of ${raw.dsh.length} give an agent tools or prompt guidance. ${n(raw.dsh, 'needs a DSH host')} need DSH's own app (its sessions, web server, model layer); ${n(raw.dsh, 'not applicable')} add nothing an agent uses (UI or settings). DSH's popular plugins mostly extend the DSH app rather than add agent tools.`)
}

writeFileSync(opts.out, `# Plugin compatibility

How the most-downloaded third-party plugins of two agent ecosystems load in
a yoagent agent, unchanged, through yoagent-rutis's adapters. Generated by
\`scripts/ecosystem-matrix.mjs\` on ${date}; raw results in
[\`${opts.json.split('/').pop()}\`](${opts.json.split('/').pop()}).

**What was checked.** Each package is installed on its own from npm and
loaded the way an agent would load it; the probe lists what a run would be
offered. Nothing is called: tools are listed, not run, and no model is
asked. So "works" means it loads and yoagent gets what is listed, not that
every tool was exercised.

**In short.**
${summary.join('\n')}

**Results.**
- **works**: loads; everything it registers maps to yoagent.
- **works, partly**: loads; some of what it registers is an app feature
  yoagent's agent loop has no place for (commands, shortcuts, renderers, …),
  listed as *ignored*. Its tools, policies and prompt additions work.
- **refused**: the adapter declined to load it — it handles an event
  yoagent never fires that would decide what the agent does, or its startup
  (\`session_start\`) failed, typically reaching for pi app internals such
  as its model registry. Failing closed is the intended outcome.
- **nothing to use**: loads, but offers a run no tools, hooks or prompt
  additions — everything it registers is ignored here, or, as installed, it
  registers nothing (several only do once given keys or a config file).
- **needs a DSH host**: it waits for services of DSH's own app (sessions,
  its web server, its model layer, …) or loads DSH app plugins that an
  agent library does not run.
- **not applicable**: nothing for an agent: themes, prompt templates, UI.
- **failed** / **not installed**: an error loading or installing it.

${sections.join('\n\n')}
`)
log(`wrote ${opts.out} and ${opts.json}`)
