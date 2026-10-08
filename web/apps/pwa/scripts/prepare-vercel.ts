// Package the already-built PWA for its independent Vercel project.
// Run after `bun run build`, then `vercel deploy --prebuilt --prod --scope your-team`.
import { cpSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';

const root = new URL('../', import.meta.url);
const output = new URL('.vercel/output/', root);
rmSync(output, { recursive: true, force: true });
mkdirSync(output, { recursive: true });
cpSync(new URL('dist/', root), new URL('static/', output), { recursive: true });
// Same policy as the desktop app (apps/desktop/src/main/protocol.ts), except that relays and hosts are
// user-paired, so connect-src also allows any secure WebSocket. React style props need inline styles.
const CSP = [
  "default-src 'self'",
  "script-src 'self'",
  "style-src 'self' 'unsafe-inline'",
  "img-src 'self' data: blob:",
  "font-src 'self' data:",
  "media-src 'self' blob:",
  "connect-src 'self' wss:",
  "manifest-src 'self'",
  "worker-src 'self'",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-src 'none'",
  "frame-ancestors 'none'",
].join('; ');

// Camera: QR pairing. Microphone: voice input.
const SECURITY_HEADERS = {
  'Content-Security-Policy': CSP,
  'X-Content-Type-Options': 'nosniff',
  'Referrer-Policy': 'no-referrer',
  'Permissions-Policy': 'camera=(self), microphone=(self), geolocation=(), payment=(), usb=(), serial=(), bluetooth=()',
};

writeFileSync(new URL('config.json', output), JSON.stringify({
  version: 3,
  routes: [
    { src: '/(.*)', headers: SECURITY_HEADERS, continue: true },
    { src: '/(?:|index.html|sw.js|manifest.webmanifest)', headers: { 'Cache-Control': 'no-cache' }, continue: true },
    { src: '/assets/.*', headers: { 'Cache-Control': 'public, max-age=31536000, immutable' }, continue: true },
    { handle: 'filesystem' },
  ],
}, null, 2) + '\n');
console.log('Prepared .vercel/output for vibeke-app');
