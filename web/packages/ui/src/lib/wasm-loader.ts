/** wasm-bindgen initializes a module once, but does not coalesce concurrent calls to init.
 * Share the whole import/init promise: React StrictMode can start two mounts before it resolves.
 * Replacing the WASM instance under a live Rust object would invalidate that object's pointer.
 */
export function createWasmLoader<T extends { default(): Promise<unknown> }>(importModule: (url: string) => Promise<T>) {
  const pending = new Map<string, Promise<T>>();
  return (url: string): Promise<T> => {
    const existing = pending.get(url);
    if (existing) return existing;
    const loading = (async () => {
      const module = await importModule(url);
      await module.default();
      return module;
    })().catch((error) => { pending.delete(url); throw error; });
    pending.set(url, loading);
    return loading;
  };
}
