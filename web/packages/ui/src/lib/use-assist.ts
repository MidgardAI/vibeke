// React hooks for the assistant: is it usable on this host, and one request flow per component.

import { useEffect, useMemo, useState } from 'react';
import { useApp, useHost } from '../app/hooks';
import { assistEligible, assistReady } from './assist-access';
import { AssistFlow } from './assist-flow';
import { useStore } from './store';

const STATUS_TTL_MS = 60_000;
const status = new Map<string, { at: number; ready: boolean }>();
const inflight = new Map<string, Promise<boolean>>();

/** The host's `assistant.status` as usable / not, cached for a minute and shared by callers. */
export function assistantReady(request: () => Promise<{ enabled?: unknown; configured?: unknown }>, host: string): Promise<boolean> | boolean {
  const hit = status.get(host);
  if (hit && Date.now() - hit.at < STATUS_TTL_MS) return hit.ready;
  let p = inflight.get(host);
  if (!p) {
    p = request()
      .then(assistReady, () => false)
      .then((ready) => {
        status.set(host, { at: Date.now(), ready });
        inflight.delete(host);
        return ready;
      });
    inflight.set(host, p);
  }
  return p;
}

/** True when this device may use the assistant here and the host has it on. False hides the feature. */
export function useAssistAvailable(hostId: string): boolean {
  const app = useApp();
  const host = useHost(hostId);
  const online = host?.status === 'online';
  const eligible = !!host && online && assistEligible({ info: host.info, record: host.record });
  const [ready, setReady] = useState<boolean>(() => status.get(hostId)?.ready ?? false);
  useEffect(() => {
    if (!eligible) return setReady(false);
    let live = true;
    const conn = app.conn(hostId);
    if (!conn) return;
    const r = assistantReady(() => conn.request('assistant.status', {}), hostId);
    if (typeof r === 'boolean') setReady(r);
    else void r.then((v) => live && setReady(v));
    return () => {
      live = false;
    };
  }, [app, hostId, eligible]);
  return eligible && ready;
}

/** One assistant request flow for the component's lifetime (cancelled on unmount). */
export function useAssistFlow(hostId: string): { flow: AssistFlow; state: ReturnType<AssistFlow['store']['get']> } {
  const app = useApp();
  const flow = useMemo(() => new AssistFlow(() => app.conn(hostId)), [app, hostId]);
  useEffect(() => () => flow.cancel(), [flow]);
  const state = useStore(flow.store);
  return { flow, state };
}
