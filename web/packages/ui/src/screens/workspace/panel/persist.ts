// Per-workspace panel memory (collapsed tree folders, the Commits section) in localStorage.
// Storage may be missing or throw (private windows); the panel then just forgets.

import { useCallback, useEffect, useState } from 'react';

const PREFIX = 'vk.panel.';
const MAX = 400;

function read(key: string): string[] {
  try {
    const v = JSON.parse(globalThis.localStorage?.getItem(PREFIX + key) ?? '[]') as unknown;
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
  } catch {
    return [];
  }
}

function write(key: string, v: readonly string[]): void {
  try {
    if (v.length) globalThis.localStorage?.setItem(PREFIX + key, JSON.stringify(v.slice(-MAX)));
    else globalThis.localStorage?.removeItem(PREFIX + key);
  } catch {
    // ignore
  }
}

/** A remembered set of strings under `key` (null key = in-memory only). */
export function usePersistedSet(key: string | null): [ReadonlySet<string>, (next: ReadonlySet<string>) => void] {
  const [set, setSet] = useState<ReadonlySet<string>>(() => new Set(key ? read(key) : []));
  useEffect(() => setSet(new Set(key ? read(key) : [])), [key]);
  const update = useCallback(
    (next: ReadonlySet<string>) => {
      setSet(next);
      if (key) write(key, [...next]);
    },
    [key],
  );
  return [set, update];
}

export function toggled(set: ReadonlySet<string>, v: string, on?: boolean): Set<string> {
  const next = new Set(set);
  if (on ?? !next.has(v)) next.add(v);
  else next.delete(v);
  return next;
}
