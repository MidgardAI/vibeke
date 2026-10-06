// Generates the app icons into public/icons (run with `bun scripts/gen-icons.ts`). Pure code, no
// image tooling: a rounded tile with a "V" stroke and an amber "needs you" dot.

import { mkdirSync, writeFileSync } from 'node:fs';
import { deflateSync } from 'node:zlib';

type RGBA = [number, number, number, number];
const BG: RGBA = [15, 16, 18, 255];
const FG: RGBA = [123, 155, 255, 255];
const DOT: RGBA = [227, 165, 58, 255];

function crc32(buf: Uint8Array): number {
  let c = ~0;
  for (const b of buf) {
    c ^= b;
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c >>> 0;
}

function png(w: number, h: number, px: Uint8Array): Buffer {
  const chunk = (type: string, data: Uint8Array) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type, 'ascii'), Buffer.from(data)]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(td));
    return Buffer.concat([len, td, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) {
    raw[y * (w * 4 + 1)] = 0;
    Buffer.from(px.buffer, y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1);
  }
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw)), chunk('IEND', new Uint8Array())]);
}

const segDist = (px: number, py: number, ax: number, ay: number, bx: number, by: number) => {
  const dx = bx - ax;
  const dy = by - ay;
  const t = Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)));
  return Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
};

interface Opts {
  size: number;
  /** Rounded tile (else full bleed). */
  radius: number | null;
  /** Glyph scale (1 = default). */
  scale: number;
  mono?: boolean;
}

function draw(o: Opts): Buffer {
  const { size } = o;
  const px = new Uint8Array(size * size * 4);
  const SS = 4;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const acc = [0, 0, 0, 0];
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const u = (x + (sx + 0.5) / SS) / size;
          const v = (y + (sy + 0.5) / SS) / size;
          let c: RGBA = [0, 0, 0, 0];
          const inTile = (() => {
            if (o.radius === null) return true;
            const r = o.radius;
            const cx = Math.min(Math.max(u, r), 1 - r);
            const cy = Math.min(Math.max(v, r), 1 - r);
            return Math.hypot(u - cx, v - cy) <= r;
          })();
          if (inTile && !o.mono) c = BG;
          // V glyph in normalized coords around the centre.
          const s = o.scale;
          const gx = (u - 0.5) / s + 0.5;
          const gy = (v - 0.5) / s + 0.5;
          const th = 0.075;
          const d = Math.min(segDist(gx, gy, 0.27, 0.3, 0.5, 0.72), segDist(gx, gy, 0.73, 0.3, 0.5, 0.72));
          if (inTile && d < th) c = o.mono ? [255, 255, 255, 255] : FG;
          if (inTile && !o.mono && Math.hypot(gx - 0.76, gy - 0.25) < 0.075) c = DOT;
          for (let k = 0; k < 4; k++) acc[k]! += c[k]!;
        }
      }
      const i = (y * size + x) * 4;
      for (let k = 0; k < 4; k++) px[i + k] = Math.round(acc[k]! / (SS * SS));
    }
  }
  return png(size, size, px);
}

const out = new URL('../public/icons/', import.meta.url);
mkdirSync(out, { recursive: true });
const write = (name: string, b: Buffer | string) => writeFileSync(new URL(name, out), b);
write('icon-192.png', draw({ size: 192, radius: 0.22, scale: 1 }));
write('icon-512.png', draw({ size: 512, radius: 0.22, scale: 1 }));
write('maskable-512.png', draw({ size: 512, radius: null, scale: 0.7 }));
write('apple-touch-icon.png', draw({ size: 180, radius: null, scale: 0.85 }));
write('badge-96.png', draw({ size: 96, radius: null, scale: 1.1, mono: true }));
write(
  'icon.svg',
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><rect width="100" height="100" rx="22" fill="#0f1012"/><path d="M27 30 50 72 73 30" fill="none" stroke="#7b9bff" stroke-width="15" stroke-linecap="round" stroke-linejoin="round"/><circle cx="76" cy="25" r="7.5" fill="#e3a53a"/></svg>\n`,
);
console.log('icons written to', out.pathname);
