// The approval gate's decisions on real command lines, run by `node --test`
// (Node 22.18+ loads the .ts module directly). Each test works in a fresh
// temporary project: owner files exist, a scratch folder is named.
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, writeFileSync, realpathSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const scratch = realpathSync(mkdtempSync(join(tmpdir(), 'gate-scratch-')))
process.env.YOAGENT_SCRATCH = scratch
const { default: gate } = await import('./confirm-dangerous.ts')

/** A fresh gate in a fresh project; `run(command)` → whether it was refused (nobody to ask). */
function setup() {
  const project = mkdtempSync(join(tmpdir(), 'gate-project-'))
  mkdirSync(join(project, 'sub'))
  for (const f of ['owner.txt', 'report.md', 'existing.txt', 'sub/only.txt', 'main', 'my file.txt', '.env', 'a.md']) {
    writeFileSync(join(project, f), 'the owner\'s\n')
  }
  let handler
  gate({ on: (_event, h) => (handler = h) })
  const ctx = { cwd: project, hasUI: false }
  return {
    project,
    run: async (command) => !!(await handler({ toolName: 'bash', input: { command } }, ctx))?.block,
    write: async (toolName, file) => !!(await handler({ toolName, input: { path: join(project, file) } }, ctx))?.block,
  }
}

const ASKS = [
  '/bin/rm -f owner.txt', 'git restore owner.txt', 'git -C . reset --hard', 'printf changed 1>owner.txt',
  'rm -f report.md', 'command rm owner.txt', 'env FOO=1 rm owner.txt', 'git reset --hard HEAD~1', 'git clean -fd',
  'git checkout -- owner.txt', 'git checkout .', 'git -c core.x=y restore .', 'git branch -D old',
  'git push --force-with-lease', 'git push -f origin main', 'git stash drop', 'echo x 2>owner.txt', 'echo x &> owner.txt',
  'echo hi | tee owner.txt', 'mv a.md owner.txt', 'cp a.md owner.txt', 'rm *.html', 'sudo ls', 'find . -name x -delete',
  'chmod -R 777 .', 'cd sub && echo x > only.txt', 'cd sub && cat > only.txt <<EOF\nhello\nEOF',
  'cd sub && mv /etc/hosts only.txt', 'echo x > ~/.zshrc', 'echo x > $HOME/.zshrc', 'echo x > "my file.txt"',
  'mv existing.txt "my file.txt"', 'bash -c "rm existing.txt"', "sh -c 'rm existing.txt'", 'echo f | xargs rm',
  'xargs rm < list', 'find . -name x -exec rm {} \;', 'git rm existing.txt', 'timeout 5 rm existing.txt',
  'nice rm existing.txt', 'env -i rm existing.txt', '\\rm existing.txt', 'echo x >| existing.txt',
  'dd of=existing.txt if=/dev/zero', 'git push origin +main', 'git push origin :main', 'git checkout -f main',
  'git switch --discard-changes main', 'cd $DIR && rm x.txt', 'echo x >existing.txt', '>existing.txt',
  'echo "unclosed > existing.txt', 'cp a.md sub/only.txt',
]
const FREE = [
  'ls -la && cat owner.txt', 'echo hi >> owner.txt', 'python3 -m unittest 2>/dev/null', 'make 2>&1 | tail',
  'git status', 'git checkout -b feature', 'git restore --staged owner.txt', 'git push origin main', 'git diff',
  `rm -rf ${scratch}/pages`, `cd ${scratch} && rm -rf pages`, 'echo a > new.md', 'echo hi | tee -a owner.txt',
  'cargo test truncate', 'grep -r truncate src', 'git commit -m "Use sudo-less install"', 'git clean -n',
  'cat > INSTALL.md <<EOF\nrun: sudo apt install x\nrm -rf /\nEOF', 'mkdir -p out && mv a.md out/',
  'echo "a > b" > fresh.txt', 'cp sub/only.txt .',
]

test('destructive or overwriting commands ask', async () => {
  const { run } = setup()
  for (const command of ASKS) assert.equal(await run(command), true, `should ask: ${command}`)
})

test('everyday commands do not', async () => {
  const { run } = setup()
  for (const command of FREE) assert.equal(await run(command), false, `should be free: ${command}`)
})

test("the agent's own files stay free; a refused command's are not its own", async () => {
  const { run, project } = setup()
  assert.equal(await run('cargo test 2>&1 | tee test.log'), false)
  writeFileSync(join(project, 'test.log'), 'output\n')
  assert.equal(await run('cargo test 2>&1 | tee test.log'), false, 'its own log, again')
  assert.equal(await run('echo > a1.txt; echo > existing.txt'), true)
  writeFileSync(join(project, 'a1.txt'), 'the owner made it\n')
  assert.equal(await run('rm a1.txt'), true, 'a1.txt was never the agent\'s')
})

test('a cd is followed only where it surely applies', async () => {
  const { run } = setup()
  const S = scratch
  for (const command of [
    `(cd ${S}); rm -f owner.txt`, // the subshell's cd does not reach the rm
    `(cd ${S}) && rm -f pages`,
    `cd ${S} | true; rm -f pages`, // a cd in a pipeline runs in a subshell
    `cd ${S} || rm -f pages`, // runs only if the cd failed
    `cd ${S} & rm -f pages`, // backgrounded: a subshell
    'cd no-such-dir; rm -f pages', // the cd fails, the rm runs here
  ]) {
    assert.equal(await run(command), true, `should ask: ${command}`)
  }
  for (const command of [`(cd ${S} && rm -rf pages)`, `cd ${S}; rm -rf pages`, `cd ${S} && rm -rf pages && cd -`]) {
    assert.equal(await run(command), false, `should be free: ${command}`)
  }
})

test('heredocs are read as the shell reads them', async () => {
  const { run } = setup()
  for (const command of [
    "echo '<<EOF'\nrm -f owner.txt\nEOF", // quoted: not a heredoc, the rm is a command
    'echo "<<EOF"; rm -f owner.txt',
    "cat > x.md <<'EOF'\nsudo things\nEOF\nrm -f owner.txt", // the rm after the body
    'cat <<EOF > owner.txt\nnew\nEOF', // the redirection is on the opening line
  ]) {
    assert.equal(await run(command), true, `should ask: ${JSON.stringify(command)}`)
  }
  for (const command of ["cat > notes.md <<'EOF'\nrm -rf /\nsudo reboot\nEOF", 'cat > n2.md <<-EOF\n\tgit reset --hard\n\tEOF']) {
    assert.equal(await run(command), false, `should be free: ${JSON.stringify(command)}`)
  }
})

test('substitutions run commands: they are read like any command', async () => {
  const { run } = setup()
  for (const command of [
    'cat <<EOF\n$(rm -f owner.txt)\nEOF', // an unquoted heredoc body is expanded
    'cat <<EOF\n`rm -f owner.txt`\nEOF',
    'cat <<-EOF > fresh2.md\n\tline $(git reset --hard)\n\tEOF',
    'echo $(rm -f owner.txt)',
    'echo "$(rm -f owner.txt)"',
    'echo `rm -f owner.txt`',
    'diff <(rm -f owner.txt) a.md',
    'cat a.md > >(tee owner.txt)',
    'x=$(git reset --hard)',
    'echo $(( $(rm -f owner.txt) + 1 ))',
    'eval "rm -f owner.txt"',
    'eval $CMD',
    'cd sub && echo $(rm -f only.txt)', // run where the command runs: sub
    'echo "$(echo "nested $(rm -f owner.txt)")"',
  ]) {
    assert.equal(await run(command), true, `should ask: ${JSON.stringify(command)}`)
  }
  for (const command of [
    "cat <<'EOF'\n$(rm -f owner.txt)\nEOF", // a quoted delimiter: the body is text
    'cat <<\\EOF\n$(rm -f owner.txt)\nEOF',
    'echo "built at $(date)"',
    'echo $(git rev-parse HEAD) > rev.txt',
    `cd ${scratch} && echo $(rm -rf pages)`,
    'cat <<EOF > fresh.md\nrev: $(git rev-parse HEAD)\nEOF',
    'eval "echo hi"',
    'echo $(( 1 + 2 ))',
  ]) {
    assert.equal(await run(command), false, `should be free: ${JSON.stringify(command)}`)
  }
})

test('writes and edits', async () => {
  const { write } = setup()
  assert.equal(await write('write', 'owner.txt'), true, 'overwrite an existing file')
  assert.equal(await write('write', 'fresh.md'), false, 'a new file')
  assert.equal(await write('write', 'fresh.md'), false, 'its own file again')
  assert.equal(await write('edit', 'owner.txt'), false, 'a targeted edit')
  assert.equal(await write('edit', '.env'), true, 'a protected file')
})
