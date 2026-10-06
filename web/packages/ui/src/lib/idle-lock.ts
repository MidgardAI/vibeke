// Idle lock timing (spec 16 §9.1 Shell): visible but untouched for `idleMs` → lock (pause
// polling). No timer runs while the window is hidden or already locked; showing the window counts
// as activity and re-arms one timer for the remaining time.

import type { Clock, TimerHandle } from '@vibeke/core';
import type { Readable, ValueStore } from './store';

export class IdleLock {
  private last: number;
  private timer: TimerHandle | null = null;
  private offs: (() => void)[] = [];

  constructor(
    private readonly o: { clock: Clock; visible: Readable<boolean>; locked: ValueStore<boolean>; idleMs: number },
  ) {
    this.last = o.clock.now();
  }

  start(): this {
    this.offs.push(
      this.o.visible.subscribe(() => this.sync(true)),
      this.o.locked.subscribe(() => this.sync(true)),
    );
    this.sync(true);
    return this;
  }

  stop(): void {
    for (const off of this.offs.splice(0)) off();
    this.disarm();
  }

  /** User activity (cheap: no timer work; the pending timer re-checks when it fires). */
  touch(): void {
    this.last = this.o.clock.now();
  }

  /** Is a timer pending? (Tests, and "nothing runs while hidden".) */
  get armed(): boolean {
    return this.timer !== null;
  }

  private sync(activity: boolean): void {
    if (!this.o.visible.getSnapshot() || this.o.locked.get()) return this.disarm();
    if (activity) this.touch();
    this.arm();
  }

  private arm(): void {
    if (this.timer !== null) return;
    const rest = Math.max(0, this.last + this.o.idleMs - this.o.clock.now());
    this.timer = this.o.clock.setTimeout(() => this.fire(), rest);
  }

  private disarm(): void {
    if (this.timer !== null) this.o.clock.clearTimeout(this.timer);
    this.timer = null;
  }

  private fire(): void {
    this.timer = null;
    if (!this.o.visible.getSnapshot() || this.o.locked.get()) return;
    if (this.o.clock.now() - this.last >= this.o.idleMs) this.o.locked.set(true);
    else this.arm();
  }
}
