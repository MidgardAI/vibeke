import type { Clock, TimerHandle } from '@vibeke/core';

/** Deadline-driven protocol work. Painting may stop in a hidden tab without stopping this. */
export class TuiDriver {
  private timer: TimerHandle | null = null;
  private queued = false;
  private stopped = false;
  constructor(private readonly o: {
    clock: Clock; tick(): number | undefined; flush(): void; paint(): void; effects(): void; failed(error: unknown): void;
  }) {}
  wake = (): void => {
    if (this.queued || this.stopped) return;
    this.queued = true;
    queueMicrotask(() => {
      this.queued = false;
      if (this.stopped) return;
      if (this.timer !== null) this.o.clock.clearTimeout(this.timer);
      this.timer = null;
      try {
        const delay = this.o.tick();
        this.o.flush(); this.o.effects(); this.o.paint();
        if (delay !== undefined) this.timer = this.o.clock.setTimeout(() => { this.timer = null; this.wake(); }, Math.max(5, delay));
      } catch (error) { this.o.failed(error); }
    });
  };
  dispose(): void {
    this.stopped = true;
    if (this.timer !== null) this.o.clock.clearTimeout(this.timer);
    this.timer = null;
  }
}
