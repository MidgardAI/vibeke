// Renderer build (the shared UI + the desktop platform). Main and preload are bundled by
// scripts/build.ts (Bun.build), since they target Node/Electron rather than the browser.

import { fileURLToPath } from 'node:url';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';
import { appVersion, buildHash } from './scripts/meta.ts';

export default defineConfig({
  root: fileURLToPath(new URL('./src/renderer', import.meta.url)),
  // Relative asset URLs: the bundle is served from app://vibeke/.
  base: './',
  define: {
    __BUILD_HASH__: JSON.stringify(buildHash()),
    __APP_VERSION__: JSON.stringify(appVersion()),
  },
  plugins: [react(), tailwindcss()],
  build: {
    outDir: fileURLToPath(new URL('./out/renderer', import.meta.url)),
    emptyOutDir: true,
    target: 'chrome140',
    sourcemap: true,
    // One local bundle per window, loaded from disk: chunking buys nothing here.
    chunkSizeWarningLimit: 2000,
    // One window = one page; no need for module preload polyfills.
    modulePreload: { polyfill: false },
  },
  server: { host: '127.0.0.1', port: 5174, strictPort: true },
});
