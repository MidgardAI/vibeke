import { describe, expect, test } from 'bun:test';
import vectors from '../../../../crates/vk-e2e/tests/vectors.json';
import * as b64 from '../src/b64';
import { helloDevice, helloPair, helloText } from '../src/hello';
import { x25519Public } from '../src/keys';
import { CHUNK, CipherState, Initiator, MAX_MESSAGE, NOISE_MAX, Responder, Session } from '../src/noise';
import { FakeClock } from './helpers';

const te = new TextEncoder();
const td = new TextDecoder();
const big = Uint8Array.from({ length: 70_000 }, (_, n) => 97 + (n % 26));

interface Case {
  name: string;
  pattern: string;
  prologue: string;
  device_static_private: string;
  host_static_private: string;
  host_static_public: string;
  initiator_ephemeral: string;
  responder_ephemeral: string;
  psk: string | null;
  msg1: string;
  msg2: string;
  msg2_payload: string;
  transport: {
    small_plaintext: string;
    big_plaintext_len: number;
    initiator_frames: string[][];
    responder_frames: string[][];
  };
}

const cases = vectors.cases as Case[];
const enc = (frames: Uint8Array[]) => frames.map(b64.encode);

function keys(c: Case) {
  return {
    prologue: te.encode(c.prologue),
    dev: b64.decode(c.device_static_private),
    host: b64.decode(c.host_static_private),
    hostPub: b64.decode(c.host_static_public),
    ei: b64.decode(c.initiator_ephemeral),
    er: b64.decode(c.responder_ephemeral),
    psk: c.psk ? b64.decode(c.psk) : null,
  };
}

describe('conformance vectors (crates/vk-e2e/tests/vectors.json)', () => {
  test('both cases present', () => {
    expect(cases.map((c) => c.name)).toEqual(['ik', 'ikpsk2']);
    expect(cases[0]!.prologue).toBe(helloText(helloDevice()));
    expect(cases[1]!.prologue).toBe(helloText(helloPair('pid-1')));
    for (const c of cases) expect(c.transport.big_plaintext_len).toBe(big.length);
  });

  for (const c of cases) {
    test(`${c.name}: TS initiator matches Rust`, () => {
      const k = keys(c);
      expect(b64.encode(x25519Public(k.host))).toBe(c.host_static_public);
      const i = new Initiator({ prologue: k.prologue, localPrivate: k.dev, remotePublic: k.hostPub, psk: k.psk, ephemeral: k.ei });
      expect(b64.encode(i.writeFirst())).toBe(c.msg1);
      const { payload, session } = i.readSecond(b64.decode(c.msg2));
      expect(td.decode(payload)).toBe(c.msg2_payload);
      expect(b64.encode(session.remoteStatic)).toBe(c.host_static_public);

      // Same nonces → byte-identical transport frames.
      const [smallFrames, bigFrames] = c.transport.initiator_frames;
      expect(enc(session.encrypt(te.encode(c.transport.small_plaintext)))).toEqual(smallFrames!);
      const bf = session.encrypt(big);
      expect(bf.length).toBe(2); // 70000 = 65000 + 5000
      expect(enc(bf)).toEqual(bigFrames!);

      const back = c.transport.responder_frames[0]!.map((f) => b64.decode(f));
      let out: Uint8Array | null = null;
      for (const f of back) out = session.decrypt(f);
      expect(td.decode(out!)).toBe(c.transport.small_plaintext);
    });

    test(`${c.name}: TS responder matches Rust`, () => {
      const k = keys(c);
      const r = new Responder({ prologue: k.prologue, localPrivate: k.host, psk: k.psk, ephemeral: k.er });
      const { remoteStatic, payload } = r.readFirst(b64.decode(c.msg1));
      expect(remoteStatic).toEqual(x25519Public(k.dev));
      expect(payload.length).toBe(0);
      const { message, session } = r.writeSecond(te.encode(c.msg2_payload));
      expect(b64.encode(message)).toBe(c.msg2);

      const [smallFrames, bigFrames] = c.transport.initiator_frames.map((fs) => fs.map((f) => b64.decode(f)));
      let out: Uint8Array | null = null;
      for (const f of smallFrames!) out = session.decrypt(f);
      expect(td.decode(out!)).toBe(c.transport.small_plaintext);
      expect(session.decrypt(bigFrames![0]!)).toBeNull(); // flag 0: more
      out = session.decrypt(bigFrames![1]!);
      expect(out).toEqual(big);

      expect(enc(session.encrypt(te.encode(c.transport.small_plaintext)))).toEqual(c.transport.responder_frames[0]!);
    });
  }
});

// ---- adversarial -----------------------------------------------------------------------------

const seeded = (s: number) => Uint8Array.from({ length: 32 }, (_, i) => (s + i * 13) & 0xff);
const DEV = seeded(1);
const HOST = seeded(2);
const PROLOGUE = te.encode(helloText(helloDevice()));

function handshake(o: { iPsk?: Uint8Array; rPsk?: Uint8Array; iPrologue?: Uint8Array; rPrologue?: Uint8Array; hostPub?: Uint8Array; clock?: FakeClock } = {}) {
  const i = new Initiator({
    prologue: o.iPrologue ?? PROLOGUE,
    localPrivate: DEV,
    remotePublic: o.hostPub ?? x25519Public(HOST),
    psk: o.iPsk,
  });
  const r = new Responder({ prologue: o.rPrologue ?? PROLOGUE, localPrivate: HOST, psk: o.rPsk });
  r.readFirst(i.writeFirst());
  const { message, session: rs } = r.writeSecond(te.encode('{}'), o.clock);
  const { session: is } = i.readSecond(message, o.clock);
  return { is, rs };
}

describe('adversarial', () => {
  test('matching psk works, wrong psk fails at message 2', () => {
    expect(() => handshake({ iPsk: seeded(5), rPsk: seeded(5) })).not.toThrow();
    expect(() => handshake({ iPsk: seeded(5), rPsk: seeded(6) })).toThrow(/decrypt/);
  });

  test('psk on one side only fails', () => {
    expect(() => handshake({ iPsk: seeded(5) })).toThrow();
  });

  test('altered prologue fails', () => {
    const altered = te.encode(helloText(helloPair('pid-2')));
    expect(() => handshake({ rPrologue: altered })).toThrow(/decrypt/);
  });

  test('wrong host key fails', () => {
    expect(() => handshake({ hostPub: x25519Public(seeded(9)) })).toThrow(/decrypt/);
  });

  test('tampered message 1 fails', () => {
    const i = new Initiator({ prologue: PROLOGUE, localPrivate: DEV, remotePublic: x25519Public(HOST) });
    const m1 = i.writeFirst();
    m1[40]! ^= 1;
    const r = new Responder({ prologue: PROLOGUE, localPrivate: HOST });
    expect(() => r.readFirst(m1)).toThrow();
  });

  test('replayed frame fails', () => {
    const { is, rs } = handshake();
    const [f] = is.encrypt(te.encode('x'));
    expect(td.decode(rs.decrypt(f!)!)).toBe('x');
    expect(() => rs.decrypt(f!)).toThrow(/decrypt/);
  });

  test('reordered frames fail', () => {
    const { is, rs } = handshake();
    const [a] = is.encrypt(te.encode('a'));
    const [b] = is.encrypt(te.encode('b'));
    expect(() => rs.decrypt(b!)).toThrow(/decrypt/);
    expect(a).toBeDefined();
  });

  test('failed session stays failed', () => {
    const { is, rs } = handshake();
    const [a] = is.encrypt(te.encode('a'));
    const bad = new Uint8Array(a!);
    bad[0]! ^= 1;
    expect(() => rs.decrypt(bad)).toThrow();
    expect(() => rs.decrypt(a!)).toThrow(/session failed/);
  });

  // Raw cipher states sharing a key let us craft frames the Session would never produce.
  function rawPair(clock?: FakeClock) {
    const k = seeded(7);
    const tx = new CipherState();
    tx.initializeKey(k);
    const rx = new CipherState();
    rx.initializeKey(k);
    const unused = new CipherState();
    return { tx, rx: new Session(unused, rx, new Uint8Array(32), clock) };
  }

  test('bad flag fails', () => {
    const { tx, rx } = rawPair();
    expect(() => rx.decrypt(tx.encryptWithAd(new Uint8Array(0), Uint8Array.of(2, 120)))).toThrow(/flag 2/);
  });

  test('empty plaintext fails', () => {
    const { tx, rx } = rawPair();
    expect(() => rx.decrypt(tx.encryptWithAd(new Uint8Array(0), new Uint8Array(0)))).toThrow(/empty/);
  });

  test('empty application message is one final flag-only frame', () => {
    const { is, rs } = handshake();
    const frames = is.encrypt(new Uint8Array(0));
    expect(frames.length).toBe(1);
    expect(rs.decrypt(frames[0]!)).toEqual(new Uint8Array(0));
  });

  test('oversize message and oversize frame are rejected', () => {
    const { is, rs } = handshake();
    expect(() => is.encrypt(new Uint8Array(MAX_MESSAGE + 1))).toThrow(/too large/);
    expect(() => rs.decrypt(new Uint8Array(NOISE_MAX + 1))).toThrow(/too large/);
  });

  test('reassembly over 16 MiB fails', () => {
    const { tx, rx } = rawPair();
    const chunk = new Uint8Array(CHUNK + 1); // flag 0 + CHUNK bytes
    const n = Math.floor(MAX_MESSAGE / CHUNK);
    for (let i = 0; i < n; i++) rx.decrypt(tx.encryptWithAd(new Uint8Array(0), chunk));
    expect(() => rx.decrypt(tx.encryptWithAd(new Uint8Array(0), chunk))).toThrow(/too large/);
  });

  test('partial message older than 30 s fails', async () => {
    const clock = new FakeClock();
    const { is, rs } = handshake({ clock });
    const frames = is.encrypt(new Uint8Array(CHUNK * 2));
    expect(rs.decrypt(frames[0]!)).toBeNull();
    expect(rs.partialDeadlineIn()).toBe(30_000);
    await clock.advance(30_001);
    expect(() => rs.decrypt(frames[1]!)).toThrow(/deadline/);
  });

  test('chunk sizes stay under the Noise limit', () => {
    const { is, rs } = handshake();
    const msg = Uint8Array.from({ length: 200_000 }, (_, i) => i & 0xff);
    const frames = is.encrypt(msg);
    expect(frames.length).toBe(4);
    let out: Uint8Array | null = null;
    for (const f of frames) {
      expect(f.length).toBeLessThanOrEqual(NOISE_MAX);
      out = rs.decrypt(f);
    }
    expect(out).toEqual(msg);
  });
});
