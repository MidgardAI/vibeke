// The GitHub sign-in round trip for `access: github` links (spec 20 §2.3), browser side.
//
// Start: make the invitation's key, compute the nonce, save the pending pairing in session storage
// and go to the account server. Return: `#/attest?t=…` stores the result for that invitation and
// goes back to `#/pair` or `#/review`, which pair with the attestation.

import {
  DEFAULT_ACCOUNT_SERVER,
  attestNonce,
  attestStartUrl,
  parseAttestQuery,
  saveAttestHandoff,
  savePendingAttest,
  takeAttestHandoff,
  takePendingAttest,
  x25519Public,
  type AttestResult,
  type PairingLink,
  type SmallStorage,
} from '@vibeke/core';
import type { AppModel } from '../app/model';

/** Session storage, or an in-memory stand-in where it is blocked (the round trip then fails). */
export function sessionStore(): SmallStorage {
  try {
    if (typeof sessionStorage !== 'undefined') return sessionStorage;
  } catch {
    // blocked (sandboxed frame, privacy mode)
  }
  const m = new Map<string, string>();
  return { getItem: (k) => m.get(k) ?? null, setItem: (k, v) => void m.set(k, v), removeItem: (k) => void m.delete(k) };
}

/** The account server for a link: the link's own, else the shell's, else the default. */
export const accountServerFor = (app: AppModel, link: PairingLink): string => link.share?.account ?? app.platform.accountServer ?? DEFAULT_ACCOUNT_SERVER;

/** The app origin the account server returns to (`<origin>/`). */
const returnTo = (): string => `${window.location.origin}/`;

/**
 * Leave for the account server's GitHub sign-in. `d` is the link's `d` value, `route` the screen
 * that resumes. Resolves only if the browser does not navigate (it normally does).
 */
export async function startAttestation(app: AppModel, link: PairingLink, d: string, route: 'pair' | 'review'): Promise<void> {
  const key = await app.invitationKey(link);
  if (!key) throw new Error('this app cannot sign in with GitHub; open the link in a browser');
  const nonce = attestNonce(link.pid, x25519Public(key));
  savePendingAttest(sessionStore(), { route, d, pid: link.pid, nonce, at: app.platform.clock.now() });
  window.location.assign(attestStartUrl(accountServerFor(app, link), { aud: link.host, nonce, returnTo: returnTo() }));
}

/**
 * `#/attest?…`: store the result for the pending invitation and return the hash to resume, or
 * null when nothing was pending (an old or foreign redirect).
 */
export function finishAttestation(app: AppModel, query: string): string | null {
  const store = sessionStore();
  const now = app.platform.clock.now();
  const pending = takePendingAttest(store, now);
  if (!pending) return null;
  const result: AttestResult = parseAttestQuery(query) ?? { ok: false, error: 'invalid' };
  saveAttestHandoff(store, { pid: pending.pid, result, at: now });
  return `#/${pending.route}?d=${pending.d}`;
}

/** The attestation result waiting for this invitation (read once). */
export const takeAttestation = (app: AppModel, pid: string): AttestResult | null => takeAttestHandoff(sessionStore(), pid, app.platform.clock.now());
