// A line to show when the file viewer opens a file (a `src/a.ts:42` link in output). The route
// carries the file; the line is handed over here once and consumed by the viewer.

/** A request older than this was never picked up (the viewer did not open); drop it. */
const STALE_MS = 5000;

let pending: { path: string; line: number; at: number } | null = null;

export function requestFileLine(path: string, line: number | undefined): void {
  pending = line && line > 0 ? { path, line, at: Date.now() } : null;
}

/** The requested line for `path`, once. */
export function takeFileLine(path: string): number | null {
  if (!pending || pending.path !== path) return null;
  const { line, at } = pending;
  pending = null;
  return Date.now() - at < STALE_MS ? line : null;
}
