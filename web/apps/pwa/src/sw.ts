/// <reference lib="webworker" />
// Service worker (spec 16 §7.8, §8.3): precached shell with index.html navigation fallback,
// visible Web Push notifications (and `clear` pushes that only close them), notification taps that
// focus the app on the deep link, and the Web Share Target. A new version waits until the app asks
// for it (SKIP_WAITING message); there is no silent skipWaiting.

import { clientsClaim } from 'workbox-core';
import { createHandlerBoundToURL, precacheAndRoute } from 'workbox-precaching';
import { NavigationRoute, registerRoute } from 'workbox-routing';
import { isAppleWebKit } from './detect';
import { openTarget, parseClear, parsePushData, planNotification, windowShowsTarget, type WindowState } from './push-payload';
import { idbAll, idbDelete, idbSet } from './idb';
import { isExpired, isSharedRecord, newShareId, recordFromForm } from './share-store';

declare const self: ServiceWorkerGlobalScope & { __WB_MANIFEST: (string | { url: string; revision: string | null })[] };

precacheAndRoute(self.__WB_MANIFEST);
registerRoute(new NavigationRoute(createHandlerBoundToURL('index.html')));

clientsClaim();
self.addEventListener('message', (e: ExtendableMessageEvent) => {
  if ((e.data as { type?: string } | null)?.type === 'SKIP_WAITING') void self.skipWaiting();
});

const SCOPE = self.registration.scope;
const ASSETS = { icon: new URL('icons/icon-192.png', SCOPE).href, badge: new URL('icons/badge-96.png', SCOPE).href };

// WebKit revokes push permission for pushes that show nothing, so on WebKit every push shows a
// notification (the gateway never sends silent ones there and skips devices with a visible lease).
// Other browsers may skip a notification the visible app already shows, and accept `clear` pushes.
const WEBKIT = isAppleWebKit(self.navigator.userAgent, self.navigator.platform, 0);

async function windowStates(): Promise<WindowState[]> {
  const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  return windows.map((c) => ({ url: c.url, visible: c.visibilityState === 'visible', focused: c.focused }));
}

self.addEventListener('push', (event: PushEvent) => {
  const raw = parsePushData(event.data);
  const clear = parseClear(raw);
  event.waitUntil(
    (async () => {
      if (clear) {
        for (const n of await self.registration.getNotifications({ tag: clear.tag })) n.close();
        return;
      }
      const plan = planNotification(raw, ASSETS);
      if (!WEBKIT && windowShowsTarget(await windowStates(), SCOPE, plan.options.data.url)) return;
      await self.registration.showNotification(plan.title, plan.options as NotificationOptions);
    })(),
  );
});

self.addEventListener('notificationclick', (event: NotificationEvent) => {
  const url = ((event.notification.data as { url?: string } | null)?.url ?? '#/inbox') as string;
  const target = openTarget(SCOPE, url);
  event.notification.close();
  // Start the window lookup now and open or focus right after it: Android drops the user gesture
  // if anything else is awaited first.
  const lookup = self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  event.waitUntil(
    (async () => {
      const windows = await lookup;
      const client = windows.find((c) => c.url.startsWith(SCOPE)) ?? windows[0];
      if (!client) {
        await self.clients.openWindow(target);
        return;
      }
      // Let the running app route in place (keeps its state).
      const focused = client.focus().catch(() => undefined);
      client.postMessage({ type: 'open', url });
      await focused;
    })(),
  );
});

// Web Share Target: keep the shared data under a one-time id and open the share screen.
registerRoute(
  ({ url }) => url.pathname === '/share-target',
  async ({ request }) => {
    const now = Date.now();
    try {
      const id = newShareId((n) => crypto.getRandomValues(new Uint8Array(n)));
      await idbSet('shared', id, recordFromForm(await request.formData(), id, now));
      // Drop leftovers nobody opened.
      for (const r of await idbAll<unknown>('shared')) if (isSharedRecord(r) && isExpired(r, now)) await idbDelete('shared', r.id);
      return Response.redirect(`${SCOPE}#/share-in/${id}`, 303);
    } catch {
      return Response.redirect(`${SCOPE}#/inbox`, 303);
    }
  },
  'POST',
);
