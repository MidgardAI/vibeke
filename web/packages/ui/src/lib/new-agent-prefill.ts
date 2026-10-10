// A one-shot hand-over from another screen to the New agent sheet: the first prompt to prefill.
// The sender calls `setNewAgentPrefill({prompt})` and then `emitUi('new-agent')`; the sheet calls
// `takeNewAgentPrefill()` when it opens and clears it.

export interface NewAgentPrefill {
  prompt: string;
}

let pending: NewAgentPrefill | null = null;

export function setNewAgentPrefill(p: NewAgentPrefill | null): void {
  pending = p;
}

export function takeNewAgentPrefill(): NewAgentPrefill | null {
  const p = pending;
  pending = null;
  return p;
}
