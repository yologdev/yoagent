// A test fixture for the probe (`tests/probe_test.rs`): a Cordis plugin that
// injects a service no DSH host provides. It never starts, while its row
// reads as started to the host; the probe finds the missing service by
// reading `inject` from this module. Not for production use.

export const name = 'yoagent-fixture-missing-service'
export const inject = ['yoagentNoSuchService']

export function apply(_ctx: unknown) {}
