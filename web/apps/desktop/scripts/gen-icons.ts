// Regenerate the Electron, Linux, and tray icons from the approved Vibeke artwork.
import { icon, monochrome, write } from '../../../scripts/brand-icons';

const root = new URL('../build/', import.meta.url);
// Keep transparent space around the terminal window for the macOS Dock.
write(root, 'icon.png', await icon(1024, 0.8));
for (const size of [16, 32, 48, 64, 128, 256, 512]) {
  write(root, `icons/${size}x${size}.png`, await icon(size));
}
write(root, 'tray/trayTemplate.png', await monochrome(16));
write(root, 'tray/trayTemplate@2x.png', await monochrome(32));
write(root, 'tray/tray.png', await icon(32));
console.log('Desktop icons exported.');
