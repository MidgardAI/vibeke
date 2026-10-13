// The review content bundle (spec 20 §3.6), served for `*.p.<app domain>`:
//   /__vk/bridge   (bridge.html + its script)  — `vite build`
//   /__vk/sw.js    (service worker, one IIFE)   — `VK_ENTRY=sw vite build`
//   /__vk/inject.js (injected page script)      — `VK_ENTRY=inject vite build`
// Fixed file names: the worker and the injected script are addressed by path.
//
// `VITE_APP_ORIGINS` (comma-separated) lists the app origins that may run the controller around
// this origin (default `https://app.vibeke.dev`). For local development add the dev app, e.g.
// `VITE_APP_ORIGINS=http://localhost:5173`, and open the app with
// `VITE_REVIEW_CONTENT_ORIGIN=http://{sid}.p.localhost:5174`.

import { defineConfig, type Plugin } from 'vite';
import { PICK_ELEMENT_FN, PICK_REGION_FN } from '../../packages/core/src/annotate-js';

const entry = process.env.VK_ENTRY;
if (entry !== undefined && entry !== 'sw' && entry !== 'inject') throw new Error(`unknown VK_ENTRY ${entry}`);

const appOrigins = (process.env.VITE_APP_ORIGINS ?? 'https://app.vibeke.dev')
  .split(',')
  .map((s) => s.trim().replace(/\/+$/, ''))
  .filter(Boolean);
for (const o of appOrigins) if (!/^https?:\/\/[a-z0-9.-]+(:\d+)?$/i.test(o)) throw new Error(`bad app origin ${o}`);

/** The page functions of core `annotate-js.ts` as real code, so the page needs no eval. */
const picks: Plugin = {
  name: 'vk-pick',
  resolveId: (id) => (id === 'virtual:vk-pick' ? '\0vk-pick' : null),
  load: (id) => (id === '\0vk-pick' ? `export const pickElement = ${PICK_ELEMENT_FN};\nexport const pickRegion = ${PICK_REGION_FN};\n` : null),
};

/** `vite preview` like the static host: clean `/__vk/bridge`, worker scope header, no caching. */
const previewLikeHost: Plugin = {
  name: 'vk-preview-host',
  configurePreviewServer(server) {
    server.middlewares.use((req, res, next) => {
      if (req.url === '/__vk/bridge' || req.url?.startsWith('/__vk/bridge?')) req.url = '/__vk/bridge.html';
      if (req.url?.startsWith('/__vk/sw.js')) {
        res.setHeader('Service-Worker-Allowed', '/');
        res.setHeader('Cache-Control', 'no-store');
      }
      next();
    });
  },
};

const define = { __VK_APP_ORIGINS__: JSON.stringify(appOrigins) };

export default defineConfig(
  entry
    ? {
        define,
        plugins: [picks],
        publicDir: false,
        build: {
          outDir: 'dist/__vk',
          emptyOutDir: false,
          target: 'es2020',
          sourcemap: false,
          lib: { entry: `src/${entry}.ts`, formats: ['iife'], name: `vk_${entry}`, fileName: () => `${entry}.js` },
        },
      }
    : {
        define,
        plugins: [picks, previewLikeHost],
        build: {
          outDir: 'dist',
          emptyOutDir: true,
          target: 'es2020',
          // Under /__vk/: the service worker never intercepts these.
          assetsDir: '__vk/assets',
          rollupOptions: { input: { index: 'index.html', bridge: '__vk/bridge.html' } },
        },
        preview: { allowedHosts: ['.localhost'] },
      },
);
