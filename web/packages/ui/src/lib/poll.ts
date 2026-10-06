// Screen-mirror polling cadence (spec 16 §9.1 / task brief): 1.5 s while the agent works, 4 s
// otherwise, a 300 ms burst ×5 after a send, paused while hidden or idle-locked.

export const POLL_WORKING_MS = 1500;
export const POLL_IDLE_MS = 4000;
export const POLL_BURST_MS = 300;
export const BURST_COUNT = 5;

export interface PollInput {
  visible: boolean;
  locked: boolean;
  online: boolean;
  working: boolean;
  /** Remaining burst polls after a send. */
  burst: number;
}

/** Delay until the next `pane.read`, or null to pause. */
export function nextPollDelay(p: PollInput): number | null {
  if (!p.visible || p.locked || !p.online) return null;
  if (p.burst > 0) return POLL_BURST_MS;
  return p.working ? POLL_WORKING_MS : POLL_IDLE_MS;
}
