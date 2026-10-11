#!/usr/bin/env bash
# Hand a task to the handoff agent example
# (integrations/yoagent-frontend/examples/handoff_agent.rs) and walk away. It
# works in the directory you run this from — the project you hand off — and
# prints a URL to follow it, answer its questions and read its reports.
#
#   mkdir research && cd research && git init
#   path/to/yoagent/scripts/try-handoff.sh --live "research the best headphones; ask my budget"
#   path/to/yoagent/scripts/try-handoff.sh --phone "…"   # serve on the LAN, for a phone
#
# Other flags go to the example: --demo, --no-dsh, --max-minutes N,
# --budget-usd X, --web ADDR. Needs Rust (cargo), Node 24+ and npm.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
for arg in "$@"; do
  case "$arg" in
    -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --live)
      [ -n "${DEEPSEEK_API_KEY:-}" ] || { echo "error: --live needs DEEPSEEK_API_KEY" >&2; exit 1; } ;;
  esac
done
# The plugin runtimes (installed once; checks Rust, Node and npm first).
"$repo/scripts/try-coding-agent.sh" --setup-only >/dev/null
exec cargo run --quiet --manifest-path "$repo/integrations/yoagent-frontend/Cargo.toml" \
  --example handoff_agent -- "$@"
