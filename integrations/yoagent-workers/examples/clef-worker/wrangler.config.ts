import { defineWranglerConfig } from "wrangler/experimental-config";

export default defineWranglerConfig({
	build: {
		command: "cargo install -q worker-build@^0.8 && worker-build --release",
	},
	types: {
		generate: false,
	},
});
