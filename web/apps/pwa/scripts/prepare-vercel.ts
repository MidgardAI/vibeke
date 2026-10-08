// Package the already-built PWA for its independent Vercel project.
// Run after `bun run build`, then `vercel deploy --prebuilt --prod --scope your-team`.
import { cpSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';

const root = new URL('../', import.meta.url);
const output = new URL('.vercel/output/', root);
rmSync(output, { recursive: true, force: true });
mkdirSync(output, { recursive: true });
cpSync(new URL('dist/', root), new URL('static/', output), { recursive: true });
writeFileSync(new URL('config.json', output), JSON.stringify({
  version: 3,
  routes: [
    { src: '/(?:|index.html|sw.js|manifest.webmanifest)', headers: { 'Cache-Control': 'no-cache' }, continue: true },
    { src: '/assets/.*', headers: { 'Cache-Control': 'public, max-age=31536000, immutable' }, continue: true },
    { handle: 'filesystem' },
  ],
}, null, 2) + '\n');
console.log('Prepared .vercel/output for vibeke-app');
