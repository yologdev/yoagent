// A test fixture for the probe (`tests/probe_test.rs`): a Cordis plugin that
// injects a service no DSH host provides. It never starts; its row reads as
// started to a host whose catalog shares only registered names (the
// probe's), and waits in rutis under rutis 0.12's default; the probe finds the missing service by
// reading `inject` from this module. Not for production use.

export const name = 'yoagent-fixture-missing-service'
export const inject = ['yoagentNoSuchService']

export function apply(_ctx: unknown) {}
