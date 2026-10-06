// Development: Vite dev server for the renderer (HMR), main + preload bundled once, Electron
// pointed at the dev server. Restart the command after changing main-process code.

import { spawn } from 'node:child_process';

const root = new URL('..', import.meta.url).pathname;
const run = (cmd: string[], env: Record<string, string> = {}) => spawn(cmd[0]!, cmd.slice(1), { cwd: root, stdio: 'inherit', env: { ...process.env, ...env } });

const build = Bun.spawnSync(['bun', 'scripts/build.ts', '--native', '--dev'], { cwd: root, stdio: ['inherit', 'inherit', 'inherit'] });
if (build.exitCode !== 0) process.exit(1);

const vite = run(['bun', 'x', 'vite', '--logLevel', 'warn']);
const url = 'http://127.0.0.1:5174';
for (let i = 0; i < 100; i++) {
  try {
    if ((await fetch(url)).ok) break;
  } catch {
    await Bun.sleep(150);
  }
}
const electron = run(['bun', 'x', 'electron', '.'], { VITE_DEV_SERVER_URL: url });
const stop = () => {
  vite.kill();
  electron.kill();
};
electron.on('exit', (code) => {
  vite.kill();
  process.exit(code ?? 0);
});
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
