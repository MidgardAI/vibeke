// Handoff helpers. Handoffs travel host to host (spec 16 §15.2): the app starts a `handoff.send`
// job on the source host and follows it; it never carries a bundle itself.

import { RpcError } from './rpc';

/**
 * `handoff.send` refused because the agent is working now (`busy`; older gateways said
 * `conflict` with a "working" message). The user may retry with `interrupt: true`.
 */
export const isHandoffBusy = (e: unknown): boolean =>
  e instanceof RpcError && (e.kind === 'busy' || (e.kind === 'conflict' && /working/i.test(e.message)));
