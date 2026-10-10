// What a pi package contains, resolved by pi's own package manager exactly as
// `pi -e <dir>` would: its manifest (`pi` key) or the conventional
// directories. Prints JSON: { extensions, skills, prompts, themes } (paths).
// Used by the ecosystem probe (examples/ecosystem_probe.rs).
//
//   node resolve-package.mjs <package dir>
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { DefaultPackageManager, SettingsManager } from '@earendil-works/pi-coding-agent'

const dir = process.argv[2]
if (!dir) {
  console.error('usage: node resolve-package.mjs <package dir>')
  process.exit(2)
}
// An empty agent directory and in-memory settings: nothing of this machine's
// pi configuration is read or written.
const agentDir = mkdtempSync(join(tmpdir(), 'pi-agent-'))
const manager = new DefaultPackageManager({
  cwd: resolve(dir),
  agentDir,
  settingsManager: SettingsManager.inMemory(),
})
let resolved
try {
  resolved = await manager.resolveExtensionSources([resolve(dir)], { temporary: true })
} finally {
  rmSync(agentDir, { recursive: true, force: true })
}
const paths = (list) => (list ?? []).filter((r) => r.enabled !== false).map((r) => r.path)
console.log(
  JSON.stringify({
    extensions: paths(resolved.extensions),
    skills: paths(resolved.skills),
    prompts: paths(resolved.prompts),
    themes: paths(resolved.themes),
  }),
)
