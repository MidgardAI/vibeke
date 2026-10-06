// Tiny external stores for useSyncExternalStore.

import { useSyncExternalStore } from 'react';

export interface Readable<T> {
  getSnapshot(): T;
  subscribe(cb: () => void): () => void;
}

export class ValueStore<T> implements Readable<T> {
  private listeners = new Set<() => void>();
  constructor(private value: T) {}
  getSnapshot = (): T => this.value;
  get(): T {
    return this.value;
  }
  set(v: T): void {
    if (Object.is(v, this.value)) return;
    this.value = v;
    for (const cb of [...this.listeners]) cb();
  }
  update(f: (v: T) => T): void {
    this.set(f(this.value));
  }
  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };
}

export function useStore<T>(s: Readable<T>): T {
  return useSyncExternalStore(s.subscribe, s.getSnapshot, s.getSnapshot);
}
