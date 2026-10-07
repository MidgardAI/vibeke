// Regenerate the PWA icons from the shared, approved Vibeke artwork.
import { icon, ico, monochrome, write } from '../../../scripts/brand-icons';

const root = new URL('../public/icons/', import.meta.url);
write(root, 'icon-192.png', await icon(192));
write(root, 'icon-512.png', await icon(512));
write(root, 'maskable-512.png', await icon(512, 0.6, true));
write(root, 'apple-touch-icon.png', await icon(180, 0.8, true));
write(root, 'favicon.ico', await ico());
write(root, 'favicon-32.png', await icon(32, 1));
write(root, 'badge-96.png', await monochrome(96, true));
console.log('PWA icons exported.');
