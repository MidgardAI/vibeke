import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';
import { VitePWA } from 'vite-plugin-pwa';

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

export default defineConfig({
  base: '/',
  define: {
    __BUILD_HASH__: JSON.stringify(process.env.VIBEKE_BUILD_HASH ?? hash),
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  plugins: [
    react(),
    tailwindcss(),
    VitePWA({
      strategies: 'injectManifest',
      srcDir: 'src',
      filename: 'sw.ts',
      injectRegister: false,
      registerType: 'autoUpdate',
      injectManifest: {
        rollupFormat: 'iife',
        globPatterns: ['**/*.{js,css,html,svg,png,ico,webmanifest}'],
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
