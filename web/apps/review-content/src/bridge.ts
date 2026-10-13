// The bridge page (`/__vk/bridge`, spec 20 §3.6), loaded hidden by the review controller. It
// registers the service worker for the whole content origin, tells the controller it is ready,
// receives the controller's MessagePort, and gives the worker a fresh channel whenever the worker
// asks (first start, and after the browser restarted it).
//
// It accepts the port only from a parent on one of the app origins (`__VK_APP_ORIGINS__`): another
// site that frames this page gets nothing to talk to.

import { REVIEW_PROTO } from '@vibeke/core/review-protocol';

let controller: MessagePort | null = null;
let registration: ServiceWorkerRegistration | null = null;

const allowedParent = (origin: string): boolean => __VK_APP_ORIGINS__.includes(origin);

/** A new channel: one end to the controller, the other to the worker. */
async function giveChannel(): Promise<void> {
  if (!controller) return;
  const reg = registration ?? (await navigator.serviceWorker.ready);
  const worker = reg.active;
  if (!worker) return;
  const ch = new MessageChannel();
  controller.postMessage({ p: REVIEW_PROTO, t: 'channel' }, [ch.port1]);
  worker.postMessage({ p: REVIEW_PROTO, t: 'channel' }, [ch.port2]);
}

window.addEventListener('message', (e) => {
  if (e.source !== window.parent || !allowedParent(e.origin)) return;
  const d = e.data as { p?: unknown; t?: unknown } | null;
  const port = e.ports[0];
  if (d?.p !== REVIEW_PROTO || d.t !== 'port' || !port) return;
  controller?.close();
  controller = port;
  port.start();
  void giveChannel();
});

navigator.serviceWorker?.addEventListener('message', (e) => {
  const d = e.data as { p?: unknown; t?: unknown } | null;
  if (d?.p === REVIEW_PROTO && d.t === 'need-channel') void giveChannel();
});
navigator.serviceWorker?.startMessages();

async function boot(): Promise<void> {
  if (window.parent === window || !navigator.serviceWorker) return;
  registration = await navigator.serviceWorker.register('/__vk/sw.js', { scope: '/' });
  registration = await navigator.serviceWorker.ready;
  // `ready` carries nothing secret; the controller checks this frame and origin itself.
  window.parent.postMessage({ p: REVIEW_PROTO, t: 'ready' }, '*');
}

void boot().catch((e) => console.warn('review bridge: service worker failed', e));
