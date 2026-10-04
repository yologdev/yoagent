# clef-worker

A [yoagent](https://crates.io/crates/yoagent) agent in a Cloudflare Worker,
with every tool call gated by Cloudflare's Clef decision model through the
Worker's Workers AI binding.

- The agent runs on DeepSeek with two tools over a fixed set of notes:
  `list_notes` (read-only) and `delete_note` (destructive; simulated here).
- Before any tool runs, yoagent's `ToolGate` asks Clef two yes/no questions —
  is this call destructive, and did the user ask for it? — and denies a
  destructive call nobody asked for. The model gets the denial as the tool's
  error and carries on.
- Clef runs through `env.AI` (`yoagent_workers::ai::clef`): no Cloudflare API
  token, no account id.

**Status:** builds for wasm32 and is linted in CI. It has not been run
against a real Workers AI binding; the first `wrangler dev` with a Cloudflare
account is its first live run.

## Run it

You need a Cloudflare account, Rust with the `wasm32-unknown-unknown` target,
Node (for `npx wrangler`), and a DeepSeek API key.

```bash
rustup target add wasm32-unknown-unknown
npx wrangler login
npx wrangler secret put DEEPSEEK_API_KEY    # the model
npx wrangler secret put RUN_TOKEN           # any string; callers must send it

npx wrangler dev        # Workers AI calls go to Cloudflare even in dev
# or: npx wrangler deploy
```

For `wrangler dev`, put the two secrets in a `.dev.vars` file instead
(`DEEPSEEK_API_KEY=...`, `RUN_TOKEN=...`; keep it out of git).

```bash
# Allowed: read-only.
curl -s localhost:8787 -H "Authorization: Bearer $RUN_TOKEN" \
  -d 'What notes do I have?'

# Allowed: destructive, but asked for.
curl -s localhost:8787 -H "Authorization: Bearer $RUN_TOKEN" \
  -d 'Delete my groceries note.'

# Should be denied: destructive, and not what was asked.
curl -s localhost:8787 -H "Authorization: Bearer $RUN_TOKEN" \
  -d 'Summarize my notes, then clean up anything that looks old.'
```

The response is JSON: the agent's `answer`, and `tools` with each call's
outcome (`"error": true` with the gate's reason when Clef denied it).

What the gate decides depends on Clef's probabilities and the thresholds
(`ToolGate::with_destructive_threshold`, `with_requested_threshold`); treat
the third prompt as an illustration, not a guarantee. The gate fails closed:
if Clef errors or times out (5 s), the call is denied. It is a guardrail, not a
security boundary.

## Size

`worker-build --release` with this crate's profile (`opt-level = "s"`, LTO):
985 KB of wasm, **about 392 KiB gzipped** with the JS shim (measured locally
with `gzip -9`; wrangler's upload figure may differ slightly). A yoagent
Worker with one provider, one tool and no decision model is about 328 KiB.
Name the provider (`Agent::from_provider`) rather than `Agent::from_config`,
which links all seven and adds about 140 KiB.

## Outside this repository

The `Cargo.toml` uses paths so CI builds it against the code beside it. In
your own Worker use the published crates:

```toml
yoagent = { version = "0.24", default-features = false, features = ["decision"] }
yoagent-workers = "0.1"
worker = "0.8"
```
