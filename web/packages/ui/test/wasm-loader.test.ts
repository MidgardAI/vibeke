import { expect, test } from 'bun:test';
import { createWasmLoader, publishedTuiModule } from '../src/lib/wasm-loader';

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

test('an old cached shell recovers once through the current module manifest', async () => {
  let inits = 0; const urls: string[] = [];
  const module = { default: async () => { inits++; } };
  const load = createWasmLoader(async (url) => { urls.push(url); if (url === 'old') throw new Error('404'); return module; }, async () => 'new');
  const [a, b] = await Promise.all([load('old'), load('old')]);
  expect(a).toBe(b); expect(urls).toEqual(['old', 'new']); expect(inits).toBe(1);
});
test('manifest fallback refuses a different origin or an incompatible bridge API', async () => {
  const url = 'https://app.test/tui/aaaaaaaaaaaaaaaa/vk_tui.js';
  const fetcher = (body: object) => (async (_url: unknown, options: RequestInit) => { expect(options.cache).toBe('no-store'); return Response.json(body); }) as typeof fetch;
  expect(await publishedTuiModule(url, fetcher({ api: 2, moduleUrl: '/tui/bbbbbbbbbbbbbbbb/vk_tui.js' }))).toBe('https://app.test/tui/bbbbbbbbbbbbbbbb/vk_tui.js');
  expect(await publishedTuiModule(url, fetcher({ api: 3, moduleUrl: '/tui/bbbbbbbbbbbbbbbb/vk_tui.js' }))).toBeNull();
  expect(await publishedTuiModule(url, fetcher({ api: 2, moduleUrl: 'https://other.test/tui/bbbbbbbbbbbbbbbb/vk_tui.js' }))).toBeNull();
});
