// Hold-to-repeat for arrow keys: one send on press, then after a delay a send every interval
// while held, at most one send in flight, and a hard cap so a stuck pointer cannot run away.

export const REPEAT_DELAY_MS = 350;
export const REPEAT_INTERVAL_MS = 90;
export const REPEAT_CAP_MS = 4000;

export interface RepeatTimers {
  set(fn: () => void, ms: number): unknown;
  clear(handle: unknown): void;
  now(): number;
}

const realTimers: RepeatTimers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
  now: () => Date.now(),
};

export class HoldRepeater {
  private timer: unknown = null;
  private startedAt = 0;
  private inFlight = false;
  private held = false;

  /** `send` resolves false when the send failed: repeating stops. */
  constructor(
    private readonly send: () => Promise<boolean>,
    private readonly timers: RepeatTimers = realTimers,
  ) {}

  get active(): boolean {
    return this.held;
  }

  start(): void {
    this.stop();
    this.held = true;
    this.startedAt = this.timers.now();
    this.fire();
    this.timer = this.timers.set(() => this.tick(), REPEAT_DELAY_MS);
  }

  stop(): void {
    this.held = false;
    if (this.timer !== null) this.timers.clear(this.timer);
    this.timer = null;
  }

  private tick(): void {
    this.timer = null;
    if (!this.held) return;
    if (this.timers.now() - this.startedAt >= REPEAT_CAP_MS) return this.stop();
    this.fire();
    this.timer = this.timers.set(() => this.tick(), REPEAT_INTERVAL_MS);
  }

  private fire(): void {
    if (this.inFlight) return;
    this.inFlight = true;
    void this.send().then(
      (ok) => {
        this.inFlight = false;
        if (!ok) this.stop();
      },
      () => {
        this.inFlight = false;
        this.stop();
      },
    );
  }
}
