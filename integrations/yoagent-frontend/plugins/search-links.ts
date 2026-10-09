// EXPERIMENTAL. A sample UI plugin: a rutis plugin that offers the browser
// frontend a component. It renders web-search tool results (DSH's
// `advanced_search` and friends) as a list of links, and adds a side panel
// with quick prompts. The component is an ES module the browser loads from
// /ui-plugins/search-links.js.

import { definePlugin } from '@arcships/rutis'

interface Frontend {
  addUiPlugin(info: { name: string; tools: string[]; panel: boolean }, module: string): () => void
}

// The browser side. Plain DOM, no dependencies; runs in the page.
const MODULE = String.raw`
const urlPattern = /https?:\/\/[^\s"'<>)\]]+/g

function textOf(result) {
  return (result?.content ?? [])
    .filter((block) => block.type === 'text')
    .map((block) => block.text)
    .join('\n')
}

export function renderTool({ result, isError }, element) {
  if (isError) return
  const text = textOf(result)
  const urls = [...new Set(text.match(urlPattern) ?? [])].slice(0, 8)
  if (urls.length === 0) return
  const list = document.createElement('ul')
  list.style.cssText = 'margin:4px 0;padding-left:18px;font-size:13px'
  for (const url of urls) {
    const item = document.createElement('li')
    const link = document.createElement('a')
    link.href = url
    link.target = '_blank'
    link.rel = 'noreferrer'
    link.textContent = url.replace(/^https?:\/\//, '').slice(0, 70)
    item.append(link)
    list.append(item)
  }
  element.append(list)
}

export function mountPanel(element, { send }) {
  const title = document.createElement('h4')
  title.textContent = 'Quick prompts'
  title.style.margin = '0 0 8px'
  element.append(title)
  for (const prompt of [
    'Search the web for the latest stable Rust version',
    'List the files here and summarise the project',
  ]) {
    const button = document.createElement('button')
    button.type = 'button'
    button.textContent = prompt
    button.style.cssText = 'display:block;width:100%;text-align:left;margin:0 0 6px;padding:8px'
    button.onclick = () => send({ type: 'prompt', text: prompt })
    element.append(button)
  }
}
`

export default definePlugin({
  inject: ['frontend'],
  apply(ctx) {
    const frontend = ctx.use<Frontend>('frontend')
    ctx.effect(
      frontend.addUiPlugin(
        { name: 'search-links', tools: ['advanced_search', 'platform_search', 'web_search'], panel: true },
        MODULE,
      ),
    )
  },
})
