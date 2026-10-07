// Key material (spec 16 §3), mirroring vk-e2e/src/keys.rs.

import { x25519 } from '@noble/curves/ed25519.js';
import { blake3 } from '@noble/hashes/blake3.js';
import { randomBytes } from '@noble/hashes/utils.js';
import { getOrCreateKey, type KeyStore } from './platform';

/** Lowercase RFC 4648 base32, no padding. */
export function base32(data: Uint8Array): string {
  const ALPHABET = 'abcdefghijklmnopqrstuvwxyz234567';
  let out = '';
  let buf = 0;
  let bits = 0;
  for (const b of data) {
    buf = ((buf << 8) | b) & 0xffff; // only the low `bits` (< 13) bits matter
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      out += ALPHABET[(buf >> bits) & 31];
    }
  }
  if (bits > 0) out += ALPHABET[(buf << (5 - bits)) & 31];
  return out;
}

/** Host id = base32 of the first 16 bytes of blake3(relay Ed25519 public key): 26 chars. */
export function hostId(relayPublic: Uint8Array): string {
  return base32(blake3(relayPublic).subarray(0, 16));
}

/** Short human fingerprint of a public key, `abcd-efgh` (blake3, first 8 base32 chars). */
export function fingerprint(publicKey: Uint8Array): string {
  const b = base32(blake3(publicKey).subarray(0, 5));
  return `${b.slice(0, 4)}-${b.slice(4, 8)}`;
}

/** X25519 public key for a private scalar (clamping happens inside the DH function). */
export function x25519Public(privateKey: Uint8Array): Uint8Array {
  return x25519.getPublicKey(privateKey);
}

export function generatePrivateKey(random: (n: number) => Uint8Array = randomBytes): Uint8Array {
  return random(32);
}

export const DEVICE_KEY_NAME = 'device_static';

/** The device's Noise static private key, created on first use (one key for all hosts). */
export async function loadOrCreateDeviceKey(
  store: KeyStore,
  random: (n: number) => Uint8Array = randomBytes,
): Promise<Uint8Array> {
  return getOrCreateKey(store, DEVICE_KEY_NAME, () => generatePrivateKey(random), (v) => v.length === 32);
}

/** Keystore name of the key made for one invitation (`pid` is the link's pairing id). */
export const invitationKeyName = (pid: string): string => `invite_static:${pid}`;

/**
 * A fresh Noise static key for accepting one share/handoff invitation, stored under
 * `invitationKeyName(pid)`. The host then holds a separate device for the invitation, so accepting
 * one never replaces this device's own (full) pairing, which shares the host's device registry.
 */
export async function createInvitationKey(
  store: KeyStore,
  pid: string,
  random: (n: number) => Uint8Array = randomBytes,
): Promise<{ name: string; key: Uint8Array }> {
  const name = invitationKeyName(pid);
  const key = await getOrCreateKey(store, name, () => generatePrivateKey(random), (v) => v.length === 32);
  return { name, key };
}

/**
 * The key to pair with for `link`: an invitation (share/handoff) gets its own key, a plain pairing
 * link uses the device key.
 */
export async function pairingKey(
  store: KeyStore,
  link: { pid: string; share?: unknown },
  deviceKey: Uint8Array,
  random: (n: number) => Uint8Array = randomBytes,
): Promise<{ devicePrivate: Uint8Array; keyName?: string }> {
  if (!link.share) return { devicePrivate: deviceKey };
  const { name, key } = await createInvitationKey(store, link.pid, random);
  return { devicePrivate: key, keyName: name };
}

/**
 * The private key a host record connects with: its own invitation key when it has one, else the
 * device key. A missing invitation key (storage cleared) is an error: falling back to the device
 * key would authenticate as a different device.
 */
export async function recordKey(store: KeyStore, record: { key?: string }, deviceKey: Uint8Array): Promise<Uint8Array> {
  if (!record.key) return deviceKey;
  const k = await store.get(record.key);
  if (!k || k.length !== 32) throw new Error('the key for this invitation is missing; accept the invitation again');
  return k;
}
