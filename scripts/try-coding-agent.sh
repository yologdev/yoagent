#!/usr/bin/env bash
# Try the coding agent example (integrations/yoagent-frontend/examples/coding_agent.rs)
# in one command: installs the plugin runtimes it needs, then runs it.
#
#   scripts/try-coding-agent.sh              # browser frontend, scripted model
#   scripts/try-coding-agent.sh --live       # DeepSeek (needs DEEPSEEK_API_KEY)
#   scripts/try-coding-agent.sh --terminal   # terminal frontend instead
#   scripts/try-coding-agent.sh --setup-only # install, don't run
#
# Other flags go to the example unchanged: --watch, --no-dsh, --demo PROMPT,
# --web ADDR. Needs Rust (cargo), Node 22.19+ and npm; macOS or Linux.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
frontend="$repo/integrations/yoagent-frontend"
rutis_plugins="$repo/integrations/yoagent-rutis/plugins"

fail() { echo "error: $*" >&2; exit 1; }

terminal=false
setup_only=false
live=false
web_given=false
args=()
for arg in "$@"; do
  case "$arg" in
    --terminal) terminal=true ;;
    --setup-only) setup_only=true ;;
    --live) live=true; args+=("$arg") ;;
    --web|--demo) web_given=true; args+=("$arg") ;;
    -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) args+=("$arg") ;;
  esac
done

# Requirements, checked before anything is installed.
case "$(uname -s)" in
  Darwin|Linux) ;;
  *) fail "macOS or Linux only (the plugin runtimes are Unix processes)" ;;
esac
command -v cargo >/dev/null || fail "cargo not found: install Rust from https://rustup.rs"
command -v node >/dev/null || fail "node not found: install Node 22.19 or newer"
command -v npm >/dev/null || fail "npm not found (it comes with Node)"
# pi needs 22.19 (rutis 0.9: 22).
node_ok=$(node -p 'const [a, b] = process.versions.node.split(".").map(Number); a > 22 || (a === 22 && b >= 19)')
[ "$node_ok" = true ] || fail "Node 22.19 or newer is needed (found $(node --version))"
if $live && [ -z "${DEEPSEEK_API_KEY:-}" ]; then
  fail "--live needs DEEPSEEK_API_KEY in the environment"
fi

# `npm ci` in each plugin directory whose install is missing or older than
# its lockfile.
install() {
  local dir=$1 marker="$1/node_modules/.package-lock.json"
  if [ -f "$marker" ] && [ ! "$dir/package-lock.json" -nt "$marker" ]; then
    echo "up to date: ${dir#"$repo"/}"
    return
  fi
  echo "installing: ${dir#"$repo"/}"
  npm ci --prefix "$dir" --no-audit --no-fund --loglevel=error
}
install "$frontend/plugins"
install "$rutis_plugins/pi"
install "$rutis_plugins/dsh"

if $setup_only; then
  echo "ready: run scripts/try-coding-agent.sh"
  exit 0
fi

# The browser frontend unless asked for the terminal (or given --web/--demo).
if ! $terminal && ! $web_given; then
  args=(--web ${args[@]+"${args[@]}"})
fi
$live || echo "(scripted model; add --live for DeepSeek)"
exec cargo run --quiet --manifest-path "$frontend/Cargo.toml" --example coding_agent -- ${args[@]+"${args[@]}"}
