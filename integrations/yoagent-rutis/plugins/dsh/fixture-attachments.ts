// A test fixture: a stand-in for dsh's `attachments` service
// (`@deepseek-ai/dsh-attachment`'s `AttachmentStore`), holding one image —
// a 1×1 PNG under the id `fixture-dot`. Only `readImage` is implemented.
// Not for production use.

import { definePlugin } from '@arcships/rutis'

const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==',
  'base64',
)

export default definePlugin({
  apply(ctx) {
    ctx.provide('attachments', {
      async readImage(ref: { attachmentId: string }) {
        if (ref.attachmentId !== 'fixture-dot') throw new Error(`no attachment ${ref.attachmentId}`)
        return { ref, data: new Uint8Array(PNG) }
      },
    })
  },
})
