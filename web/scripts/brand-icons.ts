// Export the approved artwork: trim empty space, scale, and apply platform formats.
import { mkdirSync, writeFileSync } from 'node:fs';
import sharp from 'sharp';

const source = new URL('../../design/brand/duck-in-a-shell.png', import.meta.url);
const mark = await sharp(source.pathname).trim({ threshold: 1 }).png().toBuffer();
const transparent = { r: 0, g: 0, b: 0, alpha: 0 };
export const background = '#111311';

export async function icon(size: number, scale = 0.96, tile = false): Promise<Buffer> {
  const inner = Math.round(size * scale);
  const artwork = await sharp(mark).resize(inner, inner, { fit: 'inside' }).png().toBuffer();
  return sharp({ create: { width: size, height: size, channels: 4, background: tile ? background : transparent } })
    .composite([{ input: artwork, gravity: 'centre' }]).png().toBuffer();
}

// System notification badges and macOS menu-bar templates only use alpha.
// Convert the light and colored details to alpha; retain the original contours.
export async function monochrome(size: number, white = false): Promise<Buffer> {
  const { data, info } = await sharp(mark).ensureAlpha().raw().toBuffer({ resolveWithObject: true });
  for (let i = 0; i < data.length; i += 4) {
    const detail = Math.max(data[i]!, data[i + 1]!, data[i + 2]!);
    data[i + 3] = Math.round(data[i + 3]! * Math.min(1, Math.max(0, (detail - 45) / 100)));
    data[i] = data[i + 1] = data[i + 2] = white ? 255 : 0;
  }
  return sharp(data, { raw: { width: info.width, height: info.height, channels: 4 } })
    .resize(size, size, { fit: 'contain', background: transparent }).png().toBuffer();
}

export async function ico(): Promise<Buffer> {
  const sizes = [16, 32, 48, 256];
  const frames = await Promise.all(sizes.map(size => icon(size, 1)));
  const header = Buffer.alloc(6 + frames.length * 16);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(frames.length, 4);
  let offset = header.length;
  for (const [i, frame] of frames.entries()) {
    const entry = 6 + i * 16;
    header[entry] = header[entry + 1] = sizes[i]! % 256;
    header.writeUInt16LE(1, entry + 4);
    header.writeUInt16LE(32, entry + 6);
    header.writeUInt32LE(frame.length, entry + 8);
    header.writeUInt32LE(offset, entry + 12);
    offset += frame.length;
  }
  return Buffer.concat([header, ...frames]);
}

export function write(root: URL, name: string, data: Buffer | string): void {
  const path = new URL(name, root);
  mkdirSync(new URL('.', path), { recursive: true });
  writeFileSync(path, data);
}

export async function siteIcons(): Promise<void> {
  const root = new URL('../apps/site/public/', import.meta.url);
  for (const size of [64, 128, 256, 512]) write(root, `brand/duck-${size}.png`, await icon(size));
  for (const size of [16, 32, 48]) write(root, `brand/favicon-${size}.png`, await icon(size, 1));
  write(root, 'favicon.ico', await ico());
  write(root, 'apple-touch-icon.png', await icon(180, 0.8, true));
  write(root, 'brand/icon-192.png', await icon(192, 0.8, true));
  write(root, 'brand/icon-512.png', await icon(512, 0.8, true));
  write(root, 'brand/maskable-512.png', await icon(512, 0.6, true));
  write(root, 'site.webmanifest', JSON.stringify({
    name: 'Vibeke', short_name: 'Vibeke', start_url: '/', display: 'browser',
    background_color: background, theme_color: background,
    icons: [
      { src: '/brand/icon-192.png', sizes: '192x192', type: 'image/png' },
      { src: '/brand/icon-512.png', sizes: '512x512', type: 'image/png' },
      { src: '/brand/maskable-512.png', sizes: '512x512', type: 'image/png', purpose: 'maskable' },
    ],
  }, null, 2) + '\n');

  // A native layout for link previews, using the same approved raster mark.
  const layout = Buffer.from(`<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="630">
    <rect width="1200" height="630" fill="${background}"/>
    <path d="M64 72H1136 M64 558H1136" stroke="#343a32"/>
    <text x="64" y="220" fill="#f0eee7" font-family="monospace" font-size="64" font-weight="bold">vibeke</text>
    <text x="64" y="318" fill="#f0eee7" font-family="sans-serif" font-size="48">Run your agents.</text>
    <text x="64" y="380" fill="#eeb978" font-family="sans-serif" font-size="48">Keep control.</text>
    <text x="64" y="510" fill="#a0a79b" font-family="monospace" font-size="22">vibeke.dev</text>
  </svg>`);
  const socialMark = await icon(480);
  write(root, 'brand/social.png', await sharp(layout).composite([{ input: socialMark, left: 666, top: 75 }]).png().toBuffer());
  console.log('Site branding exported.');
}

if (import.meta.main) await siteIcons();
