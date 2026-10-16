// A test fixture for the probe (`tests/probe_test.rs`): a Cordis plugin that
// injects a service no DSH host provides, declared `required: false`. Cordis
// requires it all the same, so it never starts; the probe must not take
// `required: false` as optional. Not for production use.

export const name = 'yoagent-fixture-optional-service'
export const inject = { yoagentNoSuchOptional: { required: false } }

export function apply(_ctx: unknown) {}
