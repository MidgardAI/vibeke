// The plaintext hello (spec 16 §4.2, §5). Its exact bytes are the Noise prologue, so the
// serialization must match serde's field order byte for byte: v, proto, mode[, pid].

export const PROTO = 'vibeke-e2e/1';
export const VERSION = 1;

export type Mode = 'pair' | 'device';

export interface Hello {
  v: number;
  proto: string;
  mode: Mode;
  /** Pairing id (pair mode only). */
  pid?: string;
}

export const helloPair = (pid: string): Hello => ({ v: VERSION, proto: PROTO, mode: 'pair', pid });
export const helloDevice = (): Hello => ({ v: VERSION, proto: PROTO, mode: 'device' });

/** Canonical text, identical to `serde_json::to_vec(&Hello)`. */
export function helloText(h: Hello): string {
  // Build a fresh object so key order is fixed regardless of how `h` was constructed.
  const o: Record<string, unknown> = { v: h.v, proto: h.proto, mode: h.mode };
  if (h.pid !== undefined) o.pid = h.pid;
  return JSON.stringify(o);
}

export const helloBytes = (h: Hello): Uint8Array => new TextEncoder().encode(helloText(h));

/** Parse and validate a received hello (mirrors `Hello::parse`). */
export function parseHello(raw: string): Hello {
  if (raw.length > 1024) throw new Error('hello: too large');
  const h = JSON.parse(raw) as Partial<Hello>;
  if (typeof h !== 'object' || h === null || typeof h.v !== 'number' || typeof h.proto !== 'string') {
    throw new Error('hello: malformed');
  }
  if (h.mode !== 'pair' && h.mode !== 'device') throw new Error('hello: bad mode');
  if (h.v !== VERSION || h.proto !== PROTO) throw new Error('unsupported_version');
  if ((h.mode === 'pair') !== (typeof h.pid === 'string')) {
    throw new Error('hello: pid required exactly in pair mode');
  }
  return h as Hello;
}
