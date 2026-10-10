// The gateway's push payload (crates/vk-gateway/src/notify.rs):
//   {title, body, tag, url, host, count, renotify}
// `url` is a hash route (`#/i/<host>/<interaction>`, `#/r/<host>/<run>`, `#/approve/<host>/<request>`,
// `#/inbox`, `#/`). Notifications never carry actions: approving needs an explicit tap in the app.
//
// Clear payload: {"kind":"clear","tag":"vibeke:<host>","host":"<host>"}. It shows nothing: the
// service worker closes the notifications with that tag (or `vibeke:<host>` when only `host` is
// set). The gateway sends it only to devices that subscribed with `supports_clear: true`, which
// the app sets on every browser except Apple WebKit (WebKit revokes push for pushes that show no
// notification).
//
// A notification is also skipped when a visible, focused app window already shows the inbox or the
// target route (never on Apple WebKit, and never when no window is visible).
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

/** The tag to close for a `clear` payload, or null when this is not one. */
export function parseClear(raw: unknown): { tag: string } | null {
  if (!raw || typeof raw !== 'object') return null;
  const p = raw as PushPayload & { kind?: unknown };
  if (p.kind !== 'clear') return null;
  const host = str(p.host, 64);
  const tag = str(p.tag, 128) ?? (host ? `vibeke:${host}` : null);
  return tag ? { tag } : null;
}

/** What the service worker knows about one open window. */
export interface WindowState {
  url: string;
  visible: boolean;
  focused: boolean;
}

/**
 * True when a visible, focused window of the app already shows the inbox or the notification's
 * target route, so a notification would only repeat what the user sees. Windows outside `scope`
 * and hidden or unfocused windows never count.
 */
export function windowShowsTarget(windows: readonly WindowState[], scope: string, targetUrl: string): boolean {
  const target = safeHashUrl(targetUrl);
  const base = scope.endsWith('/') ? scope : `${scope}/`;
  return windows.some((w) => {
    if (!w.visible || !w.focused || !w.url.startsWith(base)) return false;
    const i = w.url.indexOf('#');
    const hash = i >= 0 ? w.url.slice(i) : '#/';
    return hash === target || hash === '#/inbox' || hash.startsWith('#/inbox?');
  });
}
