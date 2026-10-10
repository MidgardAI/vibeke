// Regenerate the Electron, Linux, and tray icons from the approved Vibeke artwork.
// `--tray` only redraws the macOS menu-bar glyphs (they do not need the logo source).
import { mkdirSync, writeFileSync } from 'node:fs';
import { trayGlyph } from '../../../scripts/tray-glyph';

const root = new URL('../build/', import.meta.url);
const put = (name: string, data: Buffer) => {
  const path = new URL(name, root);
  mkdirSync(new URL('.', path), { recursive: true });
  writeFileSync(path, data);
};

// macOS menu bar: 18pt template glyphs, plain and with the "needs you" badge.
put('tray/trayTemplate.png', await trayGlyph(18));
put('tray/trayTemplate@2x.png', await trayGlyph(36));
put('tray/trayBadgeTemplate.png', await trayGlyph(18, true));
put('tray/trayBadgeTemplate@2x.png', await trayGlyph(36, true));

if (!process.argv.includes('--tray')) {
  const { icon } = await import('../../../scripts/brand-icons');
  // Keep transparent space around the terminal window for the macOS Dock.
  put('icon.png', await icon(1024, 0.8));
  for (const size of [16, 32, 48, 64, 128, 256, 512]) put(`icons/${size}x${size}.png`, await icon(size));
  put('tray/tray.png', await icon(32));
}
console.log('Desktop icons exported.');
