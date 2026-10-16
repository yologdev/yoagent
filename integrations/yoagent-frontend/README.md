# yoagent-frontend has moved to yobuild

The frontend (sessions, the browser, phone and terminal UIs, the plugin
host) and its examples now live in **[yobuild](https://github.com/yologdev/yobuild)**,
with their history. yobuild builds them on yoagent and yoagent-rutis from
crates.io.

| Was here | Now |
|---|---|
| `integrations/yoagent-frontend/` | the Rust code: [`src/frontend/`](https://github.com/yologdev/yobuild/tree/main/src/frontend) (no longer a crate of its own); its plugins and web page: [`frontend/`](https://github.com/yologdev/yobuild/tree/main/frontend) |
| `integrations/yoagent-frontend/examples/` (`coding_agent`, `handoff_agent`, `minimal_browser`) | [`examples/`](https://github.com/yologdev/yobuild/tree/main/examples) |
| `scripts/try-coding-agent.sh`, `scripts/try-handoff.sh` | [`scripts/`](https://github.com/yologdev/yobuild/tree/main/scripts) |
| `integrations/yoagent-frontend/plugins/pi-extensions/confirm-dangerous.ts` | [`frontend/plugins/pi-extensions/`](https://github.com/yologdev/yobuild/tree/main/frontend/plugins/pi-extensions) |

The three examples' old file paths keep a one-line pointer to their new
home. Other files' old links 404; the last version here is at [`7ca9590`](https://github.com/yologdev/yoagent/tree/7ca9590/integrations/yoagent-frontend).
