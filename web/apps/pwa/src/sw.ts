/// <reference lib="webworker" />
// Service worker (spec 16 §7.8, §8.3): precached shell with index.html navigation fallback,
// visible Web Push notifications, and notification taps that focus the app on the deep link.

import { clientsClaim } from 'workbox-core';
import { createHandlerBoundToURL, precacheAndRoute } from 'workbox-precaching';
import { NavigationRoute, registerRoute } from 'workbox-routing';
import { openTarget, parsePushData, planNotification } from './push-payload';

declare const self: ServiceWorkerGlobalScope & { __WB_MANIFEST: (string | { url: string; revision: string | null })[] };

precacheAndRoute(self.__WB_MANIFEST);
registerRoute(new NavigationRoute(createHandlerBoundToURL('index.html')));

self.addEventListener('install', () => void self.skipWaiting());
clientsClaim();
self.addEventListener('message', (e: ExtendableMessageEvent) => {
  if ((e.data as { type?: string } | null)?.type === 'SKIP_WAITING') void self.skipWaiting();
});

const SCOPE = self.registration.scope;
const ASSETS = { icon: new URL('icons/icon-192.png', SCOPE).href, badge: new URL('icons/badge-96.png', SCOPE).href };

// WebKit revokes push permission for pushes that show nothing, so every push shows a
// notification (the gateway never sends silent ones; it skips devices with a visible lease).
self.addEventListener('push', (event: PushEvent) => {
  const plan = planNotification(parsePushData(event.data), ASSETS);
  event.waitUntil(self.registration.showNotification(plan.title, plan.options as NotificationOptions));
});

self.addEventListener('notificationclick', (event: NotificationEvent) => {
  const url = ((event.notification.data as { url?: string } | null)?.url ?? '#/inbox') as string;
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
      const target = openTarget(SCOPE, url);
      const client = windows.find((c) => c.url.startsWith(SCOPE)) ?? windows[0];
      if (client) {
        // Let the running app route in place (keeps its state); focus first for user activation.
        await client.focus().catch(() => undefined);
        client.postMessage({ type: 'open', url });
      } else {
        await self.clients.openWindow(target);
      }
      event.notification.close();
    })(),
  );
});
