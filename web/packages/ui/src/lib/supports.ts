// Feature detection for host methods: older hosts answer `method_not_found`; remember that per
// host for the session so the UI hides the feature instead of retrying it.

import { RpcError } from '@vibeke/core';

const missing = new Set<string>();

/** The error says the host does not know the method (JSON-RPC -32601 / `method_not_found`). */
export function isMethodNotFound(e: unknown): boolean {
  return e instanceof RpcError && (e.code === -32601 || e.kind === 'method_not_found');
}

/** False once `method` failed with method_not_found on `host` (until reload). */
export function supported(host: string, method: string): boolean {
  return !missing.has(`${host}\u0000${method}`);
}

/** Record a failure; returns true when it means "unsupported". */
export function noteUnsupported(host: string, method: string, e: unknown): boolean {
  if (!isMethodNotFound(e)) return false;
  missing.add(`${host}\u0000${method}`);
  return true;
}
