import { execSync } from 'node:child_process';
import { readFileSync, rmSync } from 'node:fs';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';
import { VitePWA } from 'vite-plugin-pwa';
import { tuiSourceDigest } from '../../scripts/tui-source';
import { BROWSER_TUI_API } from '../../packages/ui/src/lib/tui-api';

const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')) as { version: string };
const hash = (() => {
  try {
    const h = execSync('git rev-parse --short=12 HEAD', { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
    const dirty = execSync('git status --porcelain -- .', { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
    return dirty ? `${h}-dirty` : h;
  } catch {
    return 'dev';
  }
})();

// Android and installed PWAs offer Vibeke in the share sheet. The service worker receives the POST
// (src/sw.ts) and opens `#/share-in/<id>`.
const shareTarget = {
  action: '/share-target',
  method: 'POST' as const,
  enctype: 'multipart/form-data',
  params: {
    title: 'title',
    text: 'text',
    url: 'url',
    files: [{ name: 'files', accept: ['image/*', 'text/*', '.md', '.txt', '.log', '.json', '.diff', '.patch'] }],
  },
};
const tuiEnabled = process.env.VIBEKE_WASM_TUI === '1';
const tuiModuleUrl = (() => {
  if (!tuiEnabled) return null;
  const manifest = JSON.parse(readFileSync(new URL('./public/tui/manifest.json', import.meta.url), 'utf8')) as { api: number; moduleUrl: string; sourceRevision: string; sourceDigest: string };
  const revision = execSync('git rev-parse HEAD').toString().trim();
  if (manifest.api !== BROWSER_TUI_API || manifest.sourceRevision !== revision || manifest.sourceDigest !== tuiSourceDigest()) throw new Error('The WASM TUI is stale. Run bun run build:tui from web/ before building this app.');
  return manifest.moduleUrl;
})();

export default defineConfig({
  base: '/',
  define: {
    __WASM_TUI_MODULE_URL__: JSON.stringify(tuiModuleUrl),
    __BUILD_HASH__: JSON.stringify(process.env.VIBEKE_BUILD_HASH ?? hash),
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  plugins: [
    { name: 'optional-tui-assets', closeBundle() { if (!tuiEnabled) rmSync(new URL('./dist/tui/', import.meta.url), { recursive: true, force: true }); } },
    react(),
    tailwindcss(),
    VitePWA({
      strategies: 'injectManifest',
      srcDir: 'src',
      filename: 'sw.ts',
      injectRegister: false,
      registerType: 'prompt',
      injectManifest: {
        rollupFormat: 'iife',
        globPatterns: ['**/*.{js,css,html,svg,png,ico,webmanifest}'],
        // The optional WASM module loads online; keep its glue out of the offline shell.
        globIgnores: ['tui/**'],
      },
      manifest: {
        name: 'Vibeke',
        short_name: 'Vibeke',
        description: 'Your agents and terminals, end-to-end encrypted.',
        id: '/',
        start_url: '/#/',
        scope: '/',
        display: 'standalone',
        orientation: 'any',
        background_color: '#0f1012',
        theme_color: '#0f1012',
        share_target: shareTarget,
        icons: [
          { src: '/icons/icon-192.png', sizes: '192x192', type: 'image/png' },
          { src: '/icons/icon-512.png', sizes: '512x512', type: 'image/png' },
          { src: '/icons/maskable-512.png', sizes: '512x512', type: 'image/png', purpose: 'maskable' },
        ],
      },
      devOptions: { enabled: false },
    }),
  ],
  build: { target: 'es2022', sourcemap: true },
  server: { host: true, port: 5173 },
});
