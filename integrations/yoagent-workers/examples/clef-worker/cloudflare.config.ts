import { bindings, defineConfig } from "cf/config";

export default defineConfig({
	worker: {
		name: "yoagent-clef-worker",
		compatibilityDate: "2026-10-01",
		// worker-build's output (the Rust build runs from wrangler.config.ts).
		entrypoint: "build/index.js",
		env: {
			// Workers AI: Clef runs through this binding, with no API token.
			// AI models always run on Cloudflare, in `cf dev` too (and are billed).
			AI: bindings.ai({
				dev: {
					remote: true,
				},
			}),
			// Locally from .dev.vars; deployed with `cf deploy --secrets-file`.
			DEEPSEEK_API_KEY: bindings.secret(),
			RUN_TOKEN: bindings.secret(),
			// Only for `?gate=jev` (TypeSafe's Jev instead of Clef).
			TYPESAFE_API_KEY: bindings.secret(),
		},
	},
});
