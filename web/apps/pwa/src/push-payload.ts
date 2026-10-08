// The gateway's push payload (crates/vk-gateway/src/notify.rs):
//   {title, body, tag, url, host, count, renotify}
// `url` is a hash route (`#/i/<host>/<interaction>`, `#/r/<host>/<run>`, `#/approve/<host>/<request>`,
// `#/inbox`, `#/`). Notifications never carry actions: approving needs an explicit tap in the app.
// Shared by the service worker and its tests; no DOM.

export interface PushPayload {
  title?: unknown;
  body?: unknown;
  tag?: unknown;
  url?: unknown;
  host?: unknown;
  count?: unknown;
  renotify?: unknown;
}

export interface NotificationPlan {
  title: string;
  options: {
    body: string;
    tag: string;
    renotify: boolean;
    data: { url: string; host: string | null };
    icon: string;
    badge: string;
  };
}

const str = (v: unknown, max: number): string | null => (typeof v === 'string' && v.trim() ? v.slice(0, max) : null);

/** Only in-app hash routes are allowed as notification targets. */
export function safeHashUrl(v: unknown): string {
  if (typeof v !== 'string') return '#/inbox';
  const u = v.trim();
  return /^#\/[A-Za-z0-9/_\-.%?=&]*$/.test(u) ? u : '#/inbox';
}

export function planNotification(raw: unknown, assets: { icon: string; badge: string }): NotificationPlan {
  let p: PushPayload = {};
  if (raw && typeof raw === 'object') p = raw as PushPayload;
  else if (typeof raw === 'string') p = { body: raw };
  const host = str(p.host, 64);
  const tag = str(p.tag, 128) ?? (host ? `vibeke:${host}` : 'vibeke');
  return {
    title: str(p.title, 200) ?? 'Vibeke',
    options: {
      body: str(p.body, 500) ?? '',
      tag,
      // renotify requires a tag; the gateway sets it only when a new item was added.
      renotify: p.renotify === true,
      data: { url: safeHashUrl(p.url), host },
      icon: assets.icon,
      badge: assets.badge,
    },
  };
}

/** Parse PushMessageData leniently (JSON, else text). */
export function parsePushData(data: { json(): unknown; text(): string } | null | undefined): unknown {
  if (!data) return {};
  try {
    return data.json();
  } catch {
    try {
      return data.text();
    } catch {
      return {};
    }
  }
}

/** Where a notification tap should go: an absolute URL under the app scope. */
export function openTarget(scope: string, url: string): string {
  const base = scope.endsWith('/') ? scope : `${scope}/`;
  return `${base}${safeHashUrl(url)}`;
}
