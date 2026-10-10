// Cloud sandbox helpers (spec 17 §5): the sign-in retry rule shared by every screen.

import type { CloudAuthMethod } from './model';
import { RpcError } from './rpc';

/** `permission_denied` with `details.reason = "needs_auth"`: sign in, then retry the call once. */
export function needsAuth(e: unknown): { provider: string; methods: CloudAuthMethod[] } | null {
  if (!(e instanceof RpcError)) return null;
  const d = e.data?.details as { reason?: unknown; provider?: unknown; methods?: unknown } | undefined;
  if (!d || d.reason !== 'needs_auth' || typeof d.provider !== 'string') return null;
  return { provider: d.provider, methods: Array.isArray(d.methods) ? (d.methods as CloudAuthMethod[]) : [] };
}
