// A send whose answer was lost (the connection dropped) may or may not have reached the pane.
// When the user sends the same thing to the same place again soon after, the call repeats the
// first call's op_id; the client derives the idempotency key from it, so the host runs it once.

export const RESEND_WINDOW_MS = 5 * 60_000;

export class ResendMemory {
  private last: { sig: string; opId: string; at: number } | null = null;

  /** The op_id to repeat for this call, if it repeats a send with an unknown outcome. */
  opIdFor(sig: string, now: number): string | undefined {
    const l = this.last;
    if (!l || l.sig !== sig || now - l.at > RESEND_WINDOW_MS) return undefined;
    return l.opId;
  }

  /** The call's outcome is unknown: a repeat of it should reuse `opId`. */
  remember(sig: string, opId: string | undefined, now: number): void {
    this.last = opId ? { sig, opId, at: now } : null;
  }

  /** The call finished (either way): a new send is a new call. */
  settle(sig: string): void {
    if (this.last?.sig === sig) this.last = null;
  }
}

/** True when the host refused pane input because the pane's agent is moving to or from a cloud box. */
export function isPaneMoving(e: unknown): boolean {
  if (!e || typeof e !== 'object') return false;
  const x = e as { message?: unknown; data?: { details?: { reason?: unknown } } };
  if (x.data?.details?.reason === 'pane_moving') return true;
  return typeof x.message === 'string' && x.message.includes('pane_moving');
}
