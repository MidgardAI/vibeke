// Generates the desktop app icons into build/ (run `bun scripts/gen-icons.ts`; outputs are
// committed). Pure code, no image tooling, same mark as the PWA: a "V" stroke with an amber
// "needs you" dot.
//
//   build/icon.png               1024² macOS-style tile (electron-builder makes .icns/.ico from it)
//   build/icons/512x512.png …    Linux icon set
//   build/tray/trayTemplate.png  16² + @2x 32², black glyph on alpha (macOS template image)
//   build/tray/tray.png          32² coloured tray icon (Linux / Windows)

import { mkdirSync, writeFileSync } from 'node:fs';
import { deflateSync } from 'node:zlib';

type RGBA = [number, number, number, number];

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
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw, { level: 9 })), chunk('IEND', new Uint8Array())]);
}

const segDist = (px: number, py: number, ax: number, ay: number, bx: number, by: number) => {
  const dx = bx - ax;
  const dy = by - ay;
  const t = Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)));
  return Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
};

/** Superellipse ("squircle", n = 5) like macOS app tiles; returns signed-ish inside test. */
const inSquircle = (u: number, v: number, inset: number): boolean => {
  const half = 0.5 - inset;
  const x = Math.abs(u - 0.5) / half;
  const y = Math.abs(v - 0.5) / half;
  return x ** 5 + y ** 5 <= 1;
};

const mix = (a: RGBA, b: RGBA, t: number): RGBA => [0, 1, 2, 3].map((i) => a[i]! + (b[i]! - a[i]!) * t) as RGBA;

/** Alpha-composite `top` over `base` (straight alpha). */
function over(base: RGBA, top: RGBA): RGBA {
  const ta = top[3] / 255;
  const ba = base[3] / 255;
  const a = ta + ba * (1 - ta);
  if (a === 0) return [0, 0, 0, 0];
  const c = [0, 1, 2].map((i) => (top[i]! * ta + base[i]! * ba * (1 - ta)) / a);
  return [c[0]!, c[1]!, c[2]!, a * 255];
}

function render(size: number, ss: number, shade: (u: number, v: number) => RGBA): Buffer {
  const px = new Uint8Array(size * size * 4);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      // Average premultiplied samples, then un-premultiply.
      let r = 0,
        g = 0,
        b = 0,
        a = 0;
      for (let sy = 0; sy < ss; sy++)
        for (let sx = 0; sx < ss; sx++) {
          const c = shade((x + (sx + 0.5) / ss) / size, (y + (sy + 0.5) / ss) / size);
          const al = c[3] / 255;
          r += c[0] * al;
          g += c[1] * al;
          b += c[2] * al;
          a += al;
        }
      const i = (y * size + x) * 4;
      const n = ss * ss;
      px[i + 3] = Math.round((a / n) * 255);
      if (a > 0) {
        px[i] = Math.round(r / a);
        px[i + 1] = Math.round(g / a);
        px[i + 2] = Math.round(b / a);
      }
    }
  }
  return png(size, size, px);
}

// The mark in glyph coordinates (0..1): V from (0.27,0.3) → (0.5,0.72) → (0.73,0.3), dot at (0.76,0.25).
const vDist = (gx: number, gy: number) => Math.min(segDist(gx, gy, 0.27, 0.3, 0.5, 0.72), segDist(gx, gy, 0.73, 0.3, 0.5, 0.72));

function appIcon(size: number): Buffer {
  const INSET = 0.1; // Apple grid: 824/1024 tile
  const top: RGBA = [32, 35, 42, 255];
  const bottom: RGBA = [10, 11, 13, 255];
  const v1: RGBA = [150, 176, 255, 255];
  const v2: RGBA = [92, 124, 240, 255];
  const dot: RGBA = [240, 176, 64, 255];
  return render(size, size >= 512 ? 3 : 4, (u, v) => {
    let c: RGBA = [0, 0, 0, 0];
    // Soft shadow under the tile.
    if (!inSquircle(u, v - 0.012, INSET)) {
      for (const [k, a] of [
        [0.0, 60],
        [0.012, 34],
        [0.024, 14],
      ] as const)
        if (inSquircle(u, v - 0.012, INSET - k)) {
          c = [0, 0, 0, a];
          break;
        }
    }
    if (!inSquircle(u, v, INSET)) return c;
    c = mix(top, bottom, Math.min(1, Math.max(0, (v - INSET) / (1 - 2 * INSET))));
    // Hairline highlight along the top edge.
    if (!inSquircle(u, v + 0.004, INSET)) c = over(c, [255, 255, 255, 40]);
    const s = 0.78;
    const gx = (u - 0.5) / s + 0.5;
    const gy = (v - 0.5) / s + 0.5;
    if (vDist(gx, gy) < 0.072) c = mix(v1, v2, Math.min(1, Math.max(0, (gy - 0.3) / 0.42)));
    const dd = Math.hypot(gx - 0.765, gy - 0.245);
    if (dd < 0.072) c = dot;
    else if (dd < 0.1) c = over(c, [240, 176, 64, Math.round(50 * (1 - (dd - 0.072) / 0.028))]);
    return c;
  });
}

/** Menu-bar template: only alpha matters; macOS tints it for light/dark/selected. */
function trayTemplate(size: number): Buffer {
  // At 16 px the dot needs clear space from the V, so the glyph shifts left.
  return render(size, 8, (u, v) => {
    const d = Math.min(segDist(u, v, 0.16, 0.3, 0.42, 0.8), segDist(u, v, 0.68, 0.3, 0.42, 0.8));
    if (d < 0.095) return [0, 0, 0, 255];
    if (Math.hypot(u - 0.86, v - 0.17) < 0.1) return [0, 0, 0, 255];
    return [0, 0, 0, 0];
  });
}

function trayColor(size: number): Buffer {
  return render(size, 8, (u, v) => {
    if (!inSquircle(u, v, 0.02)) return [0, 0, 0, 0];
    let c: RGBA = [15, 16, 18, 255];
    if (vDist(u, v) < 0.085) c = [123, 155, 255, 255];
    if (Math.hypot(u - 0.77, v - 0.24) < 0.085) c = [227, 165, 58, 255];
    return c;
  });
}

const root = new URL('../build/', import.meta.url);
for (const d of ['', 'icons/', 'tray/']) mkdirSync(new URL(d, root), { recursive: true });
const write = (name: string, b: Buffer) => writeFileSync(new URL(name, root), b);

write('icon.png', appIcon(1024));
for (const s of [16, 32, 48, 64, 128, 256, 512]) write(`icons/${s}x${s}.png`, appIcon(s));
write('tray/trayTemplate.png', trayTemplate(16));
write('tray/trayTemplate@2x.png', trayTemplate(32));
write('tray/tray.png', trayColor(32));
console.log('icons written to', root.pathname);
