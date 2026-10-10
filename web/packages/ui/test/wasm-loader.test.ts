import { expect, test } from 'bun:test';
import { createWasmLoader } from '../src/lib/wasm-loader';

test('concurrent mounts share one WASM initialization and the same instance', async () => {
  let imports = 0; let inits = 0;
  let resolve!: () => void;
  const initialized = new Promise<void>((done) => { resolve = done; });
  const module = { default: async () => { inits++; await initialized; } };
  const load = createWasmLoader(async () => { imports++; return module; });
  const first = load('/tui/build/module.js');
  const second = load('/tui/build/module.js');
  expect(first).toBe(second);
  resolve();
  expect(await first).toBe(module);
  expect(await load('/tui/build/module.js')).toBe(module);
  expect(imports).toBe(1); expect(inits).toBe(1);
});

test('an unsuccessful load can be retried', async () => {
  let calls = 0;
  const module = { default: async () => {} };
  const load = createWasmLoader(async () => { if (++calls === 1) throw new Error('offline'); return module; });
  await expect(load('/tui/build/module.js')).rejects.toThrow('offline');
  expect(await load('/tui/build/module.js')).toBe(module);
});
