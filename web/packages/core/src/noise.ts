// Noise IK / IKpsk2 with 25519, ChaChaPoly and BLAKE2s (Noise spec rev 34), plus the vibeke-e2e/1
// transport session with chunked framing (spec 16 §4.2–§5). Mirrors vk-e2e/src/noise.rs, which
// uses `snow`; tests/vectors.json pins both implementations to the same bytes.
//
// Only the two patterns we need are implemented:
//   IK:      <- s   ...   -> e, es, s, ss   <- e, ee, se
//   IKpsk2:  <- s   ...   -> e, es, s, ss   <- e, ee, se, psk
// Each returned Uint8Array is exactly one WebSocket binary frame.

import { chacha20poly1305 } from '@noble/ciphers/chacha.js';
import { x25519 } from '@noble/curves/ed25519.js';
import { blake2s } from '@noble/hashes/blake2.js';
import { hmac } from '@noble/hashes/hmac.js';
import { randomBytes } from '@noble/hashes/utils.js';
import type { Clock } from './platform';

export const IK = 'Noise_IK_25519_ChaChaPoly_BLAKE2s';
export const IK_PSK2 = 'Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s';
/** Noise's hard limit on one message. */
export const NOISE_MAX = 65535;
/** Plaintext bytes per chunk (flag + chunk + 16-byte tag stays under NOISE_MAX). */
export const CHUNK = 65000;
/** Largest reassembled application message. */
export const MAX_MESSAGE = 16 * 1024 * 1024;
/** A partially reassembled message older than this is an error (spec 16 §5). */
export const PARTIAL_DEADLINE_MS = 30_000;

const FLAG_MORE = 0;
const FLAG_FINAL = 1;
const DHLEN = 32;
const HASHLEN = 32;
const TAGLEN = 16;
const EMPTY = new Uint8Array(0);
const enc = new TextEncoder();

export class NoiseError extends Error {
  constructor(
    message: string,
    readonly kind: 'decrypt' | 'bad' | 'too_large' | 'state' = 'bad',
  ) {
    super(message);
    this.name = 'NoiseError';
  }
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function dh(priv: Uint8Array, pub: Uint8Array): Uint8Array {
  try {
    return x25519.getSharedSecret(priv, pub);
  } catch {
    // noble rejects low-order points (all-zero output); treat as a failed handshake.
    throw new NoiseError('invalid DH public key', 'decrypt');
  }
}

const hash = (data: Uint8Array): Uint8Array => blake2s(data);
const hmacHash = (key: Uint8Array, data: Uint8Array): Uint8Array => hmac(blake2s, key, data);

/** HKDF from the Noise spec §4.3 (HMAC-BLAKE2s), returning `n` (2 or 3) outputs. */
function hkdf(ck: Uint8Array, ikm: Uint8Array, n: 2 | 3): Uint8Array[] {
  const temp = hmacHash(ck, ikm);
  const o1 = hmacHash(temp, Uint8Array.of(1));
  const o2 = hmacHash(temp, concat(o1, Uint8Array.of(2)));
  if (n === 2) return [o1, o2];
  return [o1, o2, hmacHash(temp, concat(o2, Uint8Array.of(3)))];
}

/** ChaChaPoly nonce: 4 zero bytes then the 64-bit counter little-endian. */
function nonceBytes(n: number): Uint8Array {
  const out = new Uint8Array(12);
  const v = new DataView(out.buffer);
  v.setUint32(4, n >>> 0, true);
  v.setUint32(8, Math.floor(n / 2 ** 32), true);
  return out;
}

/** Noise CipherState. Counters are JS numbers; 2^53 messages is far beyond any session. */
export class CipherState {
  private k: Uint8Array | null = null;
  private n = 0;

  initializeKey(k: Uint8Array | null): void {
    this.k = k;
    this.n = 0;
  }
  hasKey(): boolean {
    return this.k !== null;
  }
  /** Current nonce (exposed for tests). */
  get nonce(): number {
    return this.n;
  }

  encryptWithAd(ad: Uint8Array, plaintext: Uint8Array): Uint8Array {
    if (!this.k) return plaintext;
    if (this.n >= Number.MAX_SAFE_INTEGER) throw new NoiseError('nonce exhausted', 'state');
    const out = chacha20poly1305(this.k, nonceBytes(this.n), ad).encrypt(plaintext);
    this.n++;
    return out;
  }

  decryptWithAd(ad: Uint8Array, ciphertext: Uint8Array): Uint8Array {
    if (!this.k) return ciphertext;
    if (this.n >= Number.MAX_SAFE_INTEGER) throw new NoiseError('nonce exhausted', 'state');
    let out: Uint8Array;
    try {
      out = chacha20poly1305(this.k, nonceBytes(this.n), ad).decrypt(ciphertext);
    } catch {
      // The nonce is not advanced on failure (Noise §5.1); callers close the session anyway.
      throw new NoiseError('decryption failed', 'decrypt');
    }
    this.n++;
    return out;
  }
}

/** Noise SymmetricState. */
class SymmetricState {
  readonly cs = new CipherState();
  ck: Uint8Array;
  h: Uint8Array;

  constructor(protocolName: string) {
    const name = enc.encode(protocolName);
    if (name.length <= HASHLEN) {
      this.h = new Uint8Array(HASHLEN);
      this.h.set(name);
    } else {
      this.h = hash(name);
    }
    this.ck = this.h;
  }

  mixKey(ikm: Uint8Array): void {
    const [ck, k] = hkdf(this.ck, ikm, 2);
    this.ck = ck!;
    this.cs.initializeKey(k!); // HASHLEN == 32, no truncation needed
  }
  mixHash(data: Uint8Array): void {
    this.h = hash(concat(this.h, data));
  }
  mixKeyAndHash(ikm: Uint8Array): void {
    const [ck, th, k] = hkdf(this.ck, ikm, 3);
    this.ck = ck!;
    this.mixHash(th!);
    this.cs.initializeKey(k!);
  }
  encryptAndHash(plaintext: Uint8Array): Uint8Array {
    const c = this.cs.encryptWithAd(this.h, plaintext);
    this.mixHash(c);
    return c;
  }
  decryptAndHash(ciphertext: Uint8Array): Uint8Array {
    const p = this.cs.decryptWithAd(this.h, ciphertext);
    this.mixHash(ciphertext);
    return p;
  }
  /** Returns [initiator→responder, responder→initiator] cipher states. */
  split(): [CipherState, CipherState] {
    const [k1, k2] = hkdf(this.ck, EMPTY, 2);
    const c1 = new CipherState();
    const c2 = new CipherState();
    c1.initializeKey(k1!);
    c2.initializeKey(k2!);
    return [c1, c2];
  }
}

interface KeyPair {
  priv: Uint8Array;
  pub: Uint8Array;
}

function keyPair(priv: Uint8Array): KeyPair {
  if (priv.length !== DHLEN) throw new NoiseError('private key must be 32 bytes', 'bad');
  return { priv, pub: x25519.getPublicKey(priv) };
}

function check32(k: Uint8Array, what: string): Uint8Array {
  if (k.length !== 32) throw new NoiseError(`${what} must be 32 bytes`, 'bad');
  return k;
}

/** Shared handshake machinery: symmetric state initialised with prologue and the `<- s` pre-message. */
class Handshake {
  protected ss: SymmetricState;
  protected readonly psk: Uint8Array | null;

  constructor(prologue: Uint8Array, responderStatic: Uint8Array, psk: Uint8Array | null) {
    this.psk = psk ? check32(psk, 'psk') : null;
    this.ss = new SymmetricState(psk ? IK_PSK2 : IK);
    this.ss.mixHash(prologue);
    this.ss.mixHash(responderStatic);
  }

  /** `e` token on write: in psk modes it also feeds MixKey (Noise §9.2). */
  protected writeE(e: KeyPair): Uint8Array {
    this.ss.mixHash(e.pub);
    if (this.psk) this.ss.mixKey(e.pub);
    return e.pub;
  }
  protected readE(re: Uint8Array): void {
    this.ss.mixHash(re);
    if (this.psk) this.ss.mixKey(re);
  }
}

export interface InitiatorOptions {
  prologue: Uint8Array;
  /** Device static private key. */
  localPrivate: Uint8Array;
  /** Pinned host static public key. */
  remotePublic: Uint8Array;
  /** Present selects IKpsk2 (pairing); absent selects IK. */
  psk?: Uint8Array | null;
  /** Fixed ephemeral private key — conformance tests only. */
  ephemeral?: Uint8Array;
  random?: (n: number) => Uint8Array;
}

/** Device side. */
export class Initiator extends Handshake {
  private readonly s: KeyPair;
  private readonly rs: Uint8Array;
  private readonly e: KeyPair;
  private step = 0;

  constructor(o: InitiatorOptions) {
    super(o.prologue, check32(o.remotePublic, 'remote public key'), o.psk ?? null);
    this.s = keyPair(o.localPrivate);
    this.rs = o.remotePublic;
    this.e = keyPair(o.ephemeral ?? (o.random ?? randomBytes)(32));
  }

  /** Handshake message 1 (`-> e, es, s, ss`). */
  writeFirst(payload: Uint8Array = EMPTY): Uint8Array {
    if (this.step !== 0) throw new NoiseError('writeFirst called twice', 'state');
    this.step = 1;
    const parts = [this.writeE(this.e)];
    this.ss.mixKey(dh(this.e.priv, this.rs)); // es
    parts.push(this.ss.encryptAndHash(this.s.pub)); // s
    this.ss.mixKey(dh(this.s.priv, this.rs)); // ss
    parts.push(this.ss.encryptAndHash(payload));
    const msg = concat(...parts);
    if (msg.length > NOISE_MAX) throw new NoiseError('handshake message too large', 'too_large');
    return msg;
  }

  /** Handshake message 2 (`<- e, ee, se[, psk]`) → host payload and the transport session. */
  readSecond(msg: Uint8Array, clock?: Clock): { payload: Uint8Array; session: Session } {
    if (this.step !== 1) throw new NoiseError('readSecond out of order', 'state');
    this.step = 2;
    if (msg.length > NOISE_MAX) throw new NoiseError('handshake message too large', 'too_large');
    if (msg.length < DHLEN + TAGLEN) throw new NoiseError('handshake message too short', 'decrypt');
    const re = msg.subarray(0, DHLEN);
    this.readE(re);
    this.ss.mixKey(dh(this.e.priv, re)); // ee
    this.ss.mixKey(dh(this.s.priv, re)); // se
    if (this.psk) this.ss.mixKeyAndHash(this.psk);
    const payload = this.ss.decryptAndHash(msg.subarray(DHLEN));
    const [send, recv] = this.ss.split();
    return { payload, session: new Session(send, recv, this.rs, clock) };
  }
}

export interface ResponderOptions {
  prologue: Uint8Array;
  /** Host static private key. */
  localPrivate: Uint8Array;
  psk?: Uint8Array | null;
  ephemeral?: Uint8Array;
  random?: (n: number) => Uint8Array;
}

/** Host (gateway) side; used by tests and a future local transport. */
export class Responder extends Handshake {
  private readonly s: KeyPair;
  private readonly e: KeyPair;
  private re: Uint8Array | null = null;
  private rs: Uint8Array | null = null;
  private step = 0;

  constructor(o: ResponderOptions) {
    const s = keyPair(o.localPrivate);
    super(o.prologue, s.pub, o.psk ?? null);
    this.s = s;
    this.e = keyPair(o.ephemeral ?? (o.random ?? randomBytes)(32));
  }

  /** Read message 1 → (initiator static public key, payload). Authorize the key before writeSecond. */
  readFirst(msg: Uint8Array): { remoteStatic: Uint8Array; payload: Uint8Array } {
    if (this.step !== 0) throw new NoiseError('readFirst called twice', 'state');
    this.step = 1;
    if (msg.length > NOISE_MAX) throw new NoiseError('handshake message too large', 'too_large');
    if (msg.length < DHLEN + DHLEN + TAGLEN + TAGLEN) throw new NoiseError('handshake message too short', 'decrypt');
    const re = msg.subarray(0, DHLEN);
    this.readE(re);
    this.re = re;
    this.ss.mixKey(dh(this.s.priv, re)); // es
    const rs = this.ss.decryptAndHash(msg.subarray(DHLEN, DHLEN * 2 + TAGLEN)); // s
    this.rs = rs;
    this.ss.mixKey(dh(this.s.priv, rs)); // ss
    const payload = this.ss.decryptAndHash(msg.subarray(DHLEN * 2 + TAGLEN));
    return { remoteStatic: rs, payload };
  }

  writeSecond(payload: Uint8Array = EMPTY, clock?: Clock): { message: Uint8Array; session: Session } {
    if (this.step !== 1 || !this.re || !this.rs) throw new NoiseError('writeSecond out of order', 'state');
    this.step = 2;
    const parts = [this.writeE(this.e)];
    this.ss.mixKey(dh(this.e.priv, this.re)); // ee
    this.ss.mixKey(dh(this.e.priv, this.rs)); // se
    if (this.psk) this.ss.mixKeyAndHash(this.psk);
    parts.push(this.ss.encryptAndHash(payload));
    const message = concat(...parts);
    if (message.length > NOISE_MAX) throw new NoiseError('handshake message too large', 'too_large');
    const [recv, send] = this.ss.split();
    return { message, session: new Session(send, recv, this.rs, clock) };
  }
}

/**
 * An established channel: chunked encryption and reassembly (spec 16 §5).
 * Plaintext of every Noise transport message is `flag:u8 ‖ chunk`.
 */
export class Session {
  private partial: Uint8Array[] = [];
  private partialLen = 0;
  private partialSince: number | null = null;
  private failed = false;

  constructor(
    private readonly send: CipherState,
    private readonly recv: CipherState,
    /** The remote static key (always known after IK). */
    readonly remoteStatic: Uint8Array,
    private readonly clock?: Clock,
  ) {}

  /** Encrypt one application message into one or more frames. */
  encrypt(msg: Uint8Array): Uint8Array[] {
    if (msg.length > MAX_MESSAGE) throw new NoiseError('message too large', 'too_large');
    if (msg.length === 0) return [this.encryptChunk(FLAG_FINAL, EMPTY)];
    const frames: Uint8Array[] = [];
    for (let o = 0; o < msg.length; o += CHUNK) {
      const end = Math.min(o + CHUNK, msg.length);
      frames.push(this.encryptChunk(end === msg.length ? FLAG_FINAL : FLAG_MORE, msg.subarray(o, end)));
    }
    return frames;
  }

  private encryptChunk(flag: number, chunk: Uint8Array): Uint8Array {
    const plain = new Uint8Array(chunk.length + 1);
    plain[0] = flag;
    plain.set(chunk, 1);
    return this.send.encryptWithAd(EMPTY, plain);
  }

  /**
   * Decrypt one frame. Returns the whole message once its final chunk arrives, else null.
   * Any error poisons the session: the caller must close the connection.
   */
  decrypt(frame: Uint8Array): Uint8Array | null {
    if (this.failed) throw new NoiseError('session failed', 'state');
    try {
      return this.decryptInner(frame);
    } catch (e) {
      this.failed = true;
      throw e;
    }
  }

  private decryptInner(frame: Uint8Array): Uint8Array | null {
    if (frame.length > NOISE_MAX) throw new NoiseError('frame too large', 'too_large');
    this.checkDeadline();
    const plain = this.recv.decryptWithAd(EMPTY, frame);
    if (plain.length === 0) throw new NoiseError('empty frame', 'bad');
    const flag = plain[0]!;
    if (flag !== FLAG_MORE && flag !== FLAG_FINAL) throw new NoiseError(`frame flag ${flag}`, 'bad');
    const chunk = plain.subarray(1);
    if (this.partialLen + chunk.length > MAX_MESSAGE) throw new NoiseError('message too large', 'too_large');
    if (flag === FLAG_MORE) {
      if (this.partialSince === null) this.partialSince = this.clock?.now() ?? 0;
      this.partial.push(chunk);
      this.partialLen += chunk.length;
      return null;
    }
    const out = this.partial.length === 0 ? chunk : concat(...this.partial, chunk);
    this.partial = [];
    this.partialLen = 0;
    this.partialSince = null;
    return out;
  }

  /** Milliseconds until the pending partial message expires, or null if none is pending. */
  partialDeadlineIn(): number | null {
    if (this.partialSince === null || !this.clock) return null;
    return this.partialSince + PARTIAL_DEADLINE_MS - this.clock.now();
  }

  /** Throws when a partial message has been pending for longer than 30 s. */
  checkDeadline(): void {
    const left = this.partialDeadlineIn();
    if (left !== null && left < 0) {
      this.failed = true;
      throw new NoiseError('partial message deadline exceeded', 'bad');
    }
  }

  /** Nonces of the next send/receive (tests). */
  get nonces(): { send: number; recv: number } {
    return { send: this.send.nonce, recv: this.recv.nonce };
  }
}
