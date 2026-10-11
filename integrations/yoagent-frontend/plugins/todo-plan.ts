// EXPERIMENTAL. A UI plugin for the handoff agent: draws the plan the pi
// extension @juicesharp/rpiv-todo keeps (its `todo` tool returns the whole
// task list in `details.tasks`) as a checklist under each `todo` call, so
// the owner sees where the work stands from any browser.

import { definePlugin } from '@arcships/rutis'

interface Frontend {
  addUiPlugin(info: { name: string; tools: string[]; panel: boolean }, module: string): () => void
}

// The browser side. Plain DOM, no dependencies; runs in the page.
const MODULE = String.raw`
const MARK = { completed: '✓', in_progress: '▸', pending: '○' }

export function renderTool({ result, isError }, element) {
  const tasks = result?.details?.tasks
  if (isError || !Array.isArray(tasks)) return
  const shown = tasks.filter((task) => task.status !== 'deleted')
  if (shown.length === 0) return
  const done = shown.filter((task) => task.status === 'completed').length
  const head = document.createElement('div')
  head.textContent = 'Plan · ' + done + '/' + shown.length + ' done'
  head.style.cssText = 'font-size:12px;opacity:.7;margin:4px 0 2px'
  const list = document.createElement('ul')
  list.style.cssText = 'margin:0 0 4px;padding-left:4px;list-style:none;font-size:13px'
  for (const task of shown) {
    const item = document.createElement('li')
    const mark = MARK[task.status] ?? '○'
    item.textContent = mark + ' ' + (task.status === 'in_progress' && task.activeForm ? task.activeForm : task.subject)
    if (task.status === 'completed') item.style.opacity = '.6'
    if (task.status === 'in_progress') item.style.fontWeight = '600'
    list.append(item)
  }
  element.append(head, list)
}
`

export default definePlugin({
  inject: ['frontend'],
  apply(ctx) {
    const frontend = ctx.use<Frontend>('frontend')
    ctx.effect(frontend.addUiPlugin({ name: 'todo-plan', tools: ['todo'], panel: false }, MODULE))
  },
})
