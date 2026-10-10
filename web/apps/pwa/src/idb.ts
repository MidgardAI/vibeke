// Minimal IndexedDB key-value wrapper: one database, a few object stores, promise API.

const DB_NAME = 'vibeke';
const VERSION = 2;
/** `shared`: content received through the Web Share Target, keyed by a one-time id. */
export const STORES = ['keys', 'hosts', 'mirrors', 'shared'] as const;
export type StoreName = (typeof STORES)[number];

let dbp: Promise<IDBDatabase> | null = null;

function open(): Promise<IDBDatabase> {
  dbp ??= new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, VERSION);
    req.onupgradeneeded = () => {
      for (const s of STORES) if (!req.result.objectStoreNames.contains(s)) req.result.createObjectStore(s);
    };
    req.onsuccess = () => {
      const db = req.result;
      // A newer version (opened by the service worker or another tab) must not wait on this tab.
      db.onversionchange = () => {
        db.close();
        dbp = null;
      };
      resolve(db);
    };
    req.onerror = () => reject(req.error);
    req.onblocked = () => reject(new Error('indexedDB blocked'));
  });
  return dbp;
}

function run<T>(store: StoreName, mode: IDBTransactionMode, f: (s: IDBObjectStore) => IDBRequest): Promise<T> {
  return open().then(
    (db) =>
      new Promise<T>((resolve, reject) => {
        const tx = db.transaction(store, mode);
        const req = f(tx.objectStore(store));
        tx.oncomplete = () => resolve(req.result as T);
        tx.onerror = () => reject(tx.error ?? req.error);
        tx.onabort = () => reject(tx.error ?? new Error('transaction aborted'));
      }),
  );
}

export const idbGet = <T>(store: StoreName, key: string): Promise<T | undefined> => run<T | undefined>(store, 'readonly', (s) => s.get(key));
export const idbSet = (store: StoreName, key: string, value: unknown): Promise<void> => run<unknown>(store, 'readwrite', (s) => s.put(value, key)).then(() => {});
export const idbDelete = (store: StoreName, key: string): Promise<void> => run<unknown>(store, 'readwrite', (s) => s.delete(key)).then(() => {});
export const idbAll = <T>(store: StoreName): Promise<T[]> => run<T[]>(store, 'readonly', (s) => s.getAll());

/**
 * Atomic get-or-create in ONE readwrite transaction: IndexedDB serializes readwrite transactions
 * over the same store, so two tabs racing here cannot both create a value.
 */
export function idbGetOrCreate<T>(store: StoreName, key: string, valid: (v: unknown) => v is T, make: () => T): Promise<T> {
  return open().then(
    (db) =>
      new Promise<T>((resolve, reject) => {
        const tx = db.transaction(store, 'readwrite');
        const os = tx.objectStore(store);
        let out: T;
        const req = os.get(key);
        req.onsuccess = () => {
          if (valid(req.result)) {
            out = req.result;
          } else {
            out = make();
            os.put(out, key);
          }
        };
        tx.oncomplete = () => resolve(out);
        tx.onerror = () => reject(tx.error ?? req.error);
        tx.onabort = () => reject(tx.error ?? new Error('transaction aborted'));
      }),
  );
}

/** Ask the browser not to evict our keys (best effort; Safari ignores it). */
export function persist(): void {
  void navigator.storage?.persist?.().catch(() => {});
}
