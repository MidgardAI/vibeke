// Builds the app into out/: main + preload with Bun.build (CommonJS for Electron), the renderer
// with Vite, and the tray/notification icons next to main. Usage:
//   bun scripts/build.ts            everything
//   bun scripts/build.ts --native   main + preload only (dev: the renderer comes from vite dev)

import { cpSync, mkdirSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { appVersion, buildHash } from './meta';

const root = fileURLToPath(new URL('..', import.meta.url));
const nativeOnly = process.argv.includes('--native');
const dev = process.argv.includes('--dev');

const define = {
  __APP_VERSION__: JSON.stringify(appVersion()),
  __BUILD_HASH__: JSON.stringify(buildHash()),
  'process.env.NODE_ENV': JSON.stringify(dev ? 'development' : 'production'),
};

async function bundle(entry: string, outdir: string, target: 'node' | 'browser'): Promise<void> {
  const r = await Bun.build({
    entrypoints: [`${root}${entry}`],
    outdir: `${root}${outdir}`,
    target,
    format: 'cjs',
    naming: '[name].cjs',
    // Electron provides `electron`; ws's optional native speedups stay optional.
    external: ['electron', 'bufferutil', 'utf-8-validate'],
    define,
    sourcemap: 'linked',
    minify: !dev,
  });
  if (!r.success) {
    for (const l of r.logs) console.error(l);
    process.exit(1);
  }
}

rmSync(`${root}out/main`, { recursive: true, force: true });
rmSync(`${root}out/preload`, { recursive: true, force: true });
await bundle('src/main/index.ts', 'out/main', 'node');
// electron-updater on its own: required at run time only when a packaged feed enables updates.
await bundle('src/main/updater-impl.ts', 'out/main', 'node');
await bundle('src/preload/index.ts', 'out/preload', 'browser');
mkdirSync(`${root}out/main/assets`, { recursive: true });
cpSync(`${root}build/tray`, `${root}out/main/assets`, { recursive: true });
cpSync(`${root}build/icons/512x512.png`, `${root}out/main/assets/icon.png`);

if (!nativeOnly) {
  const vite = Bun.spawnSync(['bun', 'x', 'vite', 'build', '--logLevel', 'warn'], { cwd: root, stdio: ['inherit', 'inherit', 'inherit'] });
  if (vite.exitCode !== 0) process.exit(vite.exitCode ?? 1);
}
console.log(`built ${nativeOnly ? 'main + preload' : 'main + preload + renderer'} into out/`);
