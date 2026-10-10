import { BROWSER_TUI_API } from './tui-api';

/** Share import + initialization: concurrent mounts must never replace a live WASM instance. */
export function createWasmLoader<T extends { default(): Promise<unknown> }>(
  importModule: (url: string) => Promise<T>,
  fallback?: (url: string) => Promise<string | null>,
) {
  const pending = new Map<string, Promise<T>>();
  const load = (url: string, recover = true): Promise<T> => {
    const existing = pending.get(url);
    if (existing) return existing;
    const loading = (async () => {
      const module = await importModule(url);
      await module.default();
      return module;
    })().catch(async (error: unknown) => {
      try {
        const next = recover && fallback ? await fallback(url).catch(() => null) : null;
        if (next && next !== url) return await load(next, false);
        throw error;
      } catch (failure) {
        pending.delete(url);
        throw failure;
      }
    });
    pending.set(url, loading);
    return loading;
  };
  return load;
}

/** An older cached shell may name assets removed by a deploy. Resolve the current compatible module once. */
export async function publishedTuiModule(url: string, fetcher: typeof fetch = fetch): Promise<string | null> {
  const manifestUrl = new URL('/tui/manifest.json', url);
  const response = await fetcher(manifestUrl, { cache: 'no-store', credentials: 'omit' });
  if (!response.ok) return null;
  const manifest = await response.json() as { api?: unknown; moduleUrl?: unknown };
  if (manifest.api !== BROWSER_TUI_API || typeof manifest.moduleUrl !== 'string') return null;
  const next = new URL(manifest.moduleUrl, manifestUrl);
  return next.origin === manifestUrl.origin && /^\/tui\/[a-f0-9]{16}\/vk_tui\.js$/.test(next.pathname) ? next.href : null;
}
