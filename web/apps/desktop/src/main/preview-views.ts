// Embedded preview views (spec 06 B3.5): a `WebContentsView` per shown preview, layered over the
// owning window at the rectangle its renderer reports. Each view's session routes through the
// in-app proxy (preview-proxy.ts), which sends loopback destinations through the host gateway's
// `tunnel.*` streams (preview-tunnel.ts) on that host's existing connection.
//
// Security rules for preview views:
// - their own session per host and machine (`persist:vk-preview-<hostId>-<machine>`), never the
//   app's session; no preload, sandboxed, context-isolated, no Node;
// - every permission denied except clipboard writes; WebRTC only over the proxy;
// - certificate errors accepted only for loopback hosts (the tunnel is authenticated end to end
//   and dev servers use self-signed certificates);
// - each host and machine gets its own proxy listener; a view's webContents gets the view's
//   credential, and requests without a webContents (service workers of that partition) get the
//   partition's own credential, matched by the proxy port that challenged;
// - a port the host has not allowed (`port_not_allowed`) hides the view and asks the user; only
//   their "Allow" sends `allow_port: true`, once, and the host remembers the grant.
// - `window.open` loads in the same view, `_blank` links to non-loopback sites open in the system
//   browser, and nothing navigates to `app://`, `file://` or any other non-web scheme.

import { createHash, randomBytes } from 'node:crypto';
import { BrowserWindow, WebContentsView, session as electronSession, shell, type Session, type WebContents } from 'electron';
import { TUNNEL_FEATURE, type RpcLink } from '@vibeke/core';
import { EVENT, type PreviewAction, type PreviewBounds, type PreviewOpen, type PreviewState } from '../shared/contract';
import { PreviewProxy, isLoopbackName, isLoopbackUrl, isOurProxyChallenge, type PreviewRoute } from './preview-proxy';
import { portNotAllowed, tunnelMux } from './preview-tunnel';

export interface PreviewViewsDeps {
  /** The host's live connection for `tunnel.*`, or null while it is offline. */
  link(hostId: string): RpcLink | null;
  /** The host is connected (views show an error while it is not, and reload when it is back). */
  online(hostId: string): boolean;
  /** The gateway's `hello.features` of that host. */
  features(hostId: string): readonly string[];
  /** Host connection changes (status, reconnects). */
  onHostsChanged(cb: () => void): () => void;
  log(msg: string): void;
}

interface Entry {
  id: string;
  /** The listener of this view's partition. */
  part: Partition;
  owner: WebContents;
  win: BrowserWindow;
  view: WebContentsView;
  route: PreviewRoute;
  username: string;
  password: string;
  /** The host was offline when last checked (the view is hidden and shows an error). */
  hostDown: boolean;
  /** The renderer asked for it to be shown. */
  wantVisible: boolean;
  lastUrl: string;
  error?: string;
  /** A port the host refused (`port_not_allowed`): the view hides until the user allows it. */
  blocked?: { host: string; port: number };
  /** Ports the user said "Not now" to in this view: not asked again while it is open. */
  declined: Set<number>;
}

/** One proxy listener per session partition (host + machine), with a credential for workers. */
interface Partition {
  proxy: PreviewProxy;
  route: PreviewRoute;
  worker: { username: string; password: string };
}

/** Views per owning window. */
const MAX_VIEWS_PER_WINDOW = 16;
const NAVIGABLE = new Set(['http:', 'https:']);

/** The session partition of a host and machine. Machine names are sanitized, with a hash when changed. */
export function previewPartition(hostId: string, machine: string): string {
  const m = !machine || machine === 'local' ? 'local' : machine;
  const safe = m.replace(/[^A-Za-z0-9_.-]/g, '_').slice(0, 48);
  const key = safe === m ? safe : `${safe}-${createHash('sha256').update(m).digest('hex').slice(0, 10)}`;
  return `persist:vk-preview-${hostId}-${key}`;
}

const safeWebUrl = (url: string): string | null => {
  try {
    const u = new URL(url);
    return NAVIGABLE.has(u.protocol) && !u.username && !u.password ? u.toString() : null;
  } catch {
    return null;
  }
};

export class PreviewViews {
  private readonly partitions = new Map<string, Partition>();
  /** Ports the user allowed for a route, sent once with `allow_port: true`. */
  private readonly allowOnce = new Set<string>();
  private readonly views = new Map<string, Entry>();
  private readonly byContents = new WeakMap<WebContents, Entry>();
  private readonly sessions = new WeakSet<Session>();
  private readonly configured = new Map<string, Promise<Session>>();
  private readonly watchedOwners = new WeakSet<WebContents>();
  private readonly watchedWindows = new WeakSet<BrowserWindow>();
  private offHosts: (() => void) | null = null;

  constructor(private readonly d: PreviewViewsDeps) {}

  private partitionFor(route: PreviewRoute): Partition {
    const name = previewPartition(route.hostId, route.machine);
    let part = this.partitions.get(name);
    if (!part) {
      const proxy = new PreviewProxy({
        tunnel: (r, host, port, view) => this.openTunnel(r, host, port, view),
        log: this.d.log,
      });
      const worker = proxy.addView(`sw${randomBytes(12).toString('hex')}`, route);
      part = { proxy, route: { ...route }, worker };
      this.partitions.set(name, part);
    }
    return part;
  }

  private async openTunnel(route: PreviewRoute, host: string, port: number, view: string) {
    const link = this.d.link(route.hostId);
    if (!link) throw new Error('host is not connected');
    const key = `${route.hostId}\0${route.machine}\0${port}`;
    const allow = this.allowOnce.has(key);
    try {
      const s = await tunnelMux(link).open({ ...(route.machine ? { machine: route.machine } : {}), host, port, ...(allow ? { allow_port: true } : {}) });
      if (allow) this.allowOnce.delete(key);
      return s;
    } catch (e) {
      const refused = portNotAllowed(e);
      if (refused) this.blocked(route, view, { host, port: refused.port });
      throw e;
    }
  }

  /** The host refused a port: hide the view(s) of that request and ask the user. */
  private blocked(route: PreviewRoute, view: string, b: { host: string; port: number }): void {
    const direct = this.views.get(view);
    const hit = direct ? [direct] : [...this.views.values()].filter((e) => e.route.hostId === route.hostId && e.route.machine === route.machine);
    for (const e of hit) {
      if (e.blocked?.port === b.port || e.declined.has(b.port)) continue;
      e.blocked = b;
      e.view.setVisible(false);
      this.emit(e);
    }
  }

  /**
   * The user answered the port question of a view. Allow: open the port once with `allow_port`
   * and reload. Not now: show the page as it is (that port stays refused).
   */
  allowPort(owner: WebContents, view: string, allow: boolean): void {
    const e = this.entry(owner, view);
    if (!e.blocked) return;
    if (!allow) e.declined.add(e.blocked.port);
    if (allow) this.allowOnce.add(`${e.route.hostId}\0${e.route.machine}\0${e.blocked.port}`);
    e.blocked = undefined;
    e.view.setVisible(e.wantVisible && !e.hostDown);
    if (allow && !e.hostDown) void e.view.webContents.loadURL(e.view.webContents.getURL() || e.lastUrl).catch(() => {});
    this.emit(e);
  }

  /** Preview sessions are exempt from the app's navigation guard (they have their own rules). */
  isPreviewSession(s: Session): boolean {
    return this.sessions.has(s);
  }

  /** Is this webContents a preview view? */
  isPreviewContents(wc: WebContents): boolean {
    return this.byContents.has(wc) || this.sessions.has(wc.session);
  }

  // ---- IPC operations (the caller validated the arguments and the sender) ---------------------

  async open(owner: WebContents, p: PreviewOpen): Promise<{ view: string }> {
    const win = BrowserWindow.fromWebContents(owner);
    if (!win || win.isDestroyed()) throw new Error('no window');
    if (!this.d.features(p.hostId).includes(TUNNEL_FEATURE)) {
      // The gateway offers tunnels only to full, unlimited devices (and newer hosts).
      throw Object.assign(new Error('Previews in the app need a device with full access to this host, and an up-to-date host.'), { code: 'unsupported' });
    }
    if ([...this.views.values()].filter((e) => e.owner === owner).length >= MAX_VIEWS_PER_WINDOW) {
      throw Object.assign(new Error('Too many open previews in this window.'), { code: 'busy' });
    }
    const route: PreviewRoute = { hostId: p.hostId, machine: p.machine === 'local' ? '' : p.machine };
    const part = this.partitionFor(route);
    const port = await part.proxy.start();
    const ses = await this.sessionFor(route, port);
    if (owner.isDestroyed() || win.isDestroyed()) throw new Error('window closed');

    const id = `pv${randomBytes(12).toString('hex')}`;
    const cred = part.proxy.addView(id, route);
    const view = new WebContentsView({
      webPreferences: {
        session: ses,
        sandbox: true,
        contextIsolation: true,
        nodeIntegration: false,
        nodeIntegrationInWorker: false,
        nodeIntegrationInSubFrames: false,
        webviewTag: false,
        webSecurity: true,
        allowRunningInsecureContent: false,
        safeDialogs: true,
        devTools: true,
        spellcheck: true,
      },
    });
    const e: Entry = { id, part, owner, win, view, route, username: cred.username, password: cred.password, hostDown: false, wantVisible: false, lastUrl: p.url, declined: new Set() };
    this.views.set(id, e);
    this.byContents.set(view.webContents, e);
    this.configureContents(e);
    view.setVisible(false);
    win.contentView.addChildView(view);
    this.watchOwner(owner, win);
    this.offHosts ??= this.d.onHostsChanged(() => this.hostsChanged());
    e.hostDown = !this.d.online(route.hostId);
    if (e.hostDown) e.error = 'host_offline';
    else void view.webContents.loadURL(p.url).catch(() => {});
    this.emit(e);
    return { view: id };
  }

  bounds(owner: WebContents, b: PreviewBounds): void {
    const e = this.entry(owner, b.view);
    const zoom = owner.getZoomFactor() || 1;
    e.view.setBounds({ x: Math.round(b.x * zoom), y: Math.round(b.y * zoom), width: Math.round(b.width * zoom), height: Math.round(b.height * zoom) });
    e.wantVisible = b.visible;
    e.view.setVisible(b.visible && !e.hostDown && !e.blocked && b.width > 0 && b.height > 0);
  }

  go(owner: WebContents, view: string, p: { url: string } | { action: PreviewAction }): void {
    const e = this.entry(owner, view);
    const wc = e.view.webContents;
    if ('url' in p) {
      e.lastUrl = p.url;
      if (!e.hostDown) void wc.loadURL(p.url).catch(() => {});
      this.emit(e);
      return;
    }
    switch (p.action) {
      case 'back':
        if (wc.navigationHistory.canGoBack()) wc.navigationHistory.goBack();
        break;
      case 'forward':
        if (wc.navigationHistory.canGoForward()) wc.navigationHistory.goForward();
        break;
      case 'reload':
        if (!e.hostDown) {
          if (wc.getURL()) wc.reload();
          else void wc.loadURL(e.lastUrl).catch(() => {});
        }
        break;
      case 'stop':
        wc.stop();
        break;
    }
  }

  devtools(owner: WebContents, view: string): void {
    const wc = this.entry(owner, view).view.webContents;
    if (wc.isDevToolsOpened()) wc.closeDevTools();
    else wc.openDevTools({ mode: 'detach' });
  }

  /** The page as a JPEG data URL (shown in the placeholder while an overlay hides the view). */
  async snapshot(owner: WebContents, view: string): Promise<string | null> {
    const e = this.entry(owner, view);
    if (e.hostDown || e.view.webContents.isDestroyed()) return null;
    try {
      const img = await e.view.webContents.capturePage();
      if (img.isEmpty()) return null;
      return `data:image/jpeg;base64,${img.toJPEG(80).toString('base64')}`;
    } catch {
      return null;
    }
  }

  close(owner: WebContents, view: string): void {
    const e = this.views.get(view);
    if (e && e.owner === owner) this.destroy(e);
  }

  /**
   * `app.on('login')`: answers the proxy challenge for a preview view with its own credential.
   * A request without a webContents (a service worker of a preview page) gets the credential of
   * the partition whose proxy port challenged it. Returns true when handled (the caller must have
   * called `event.preventDefault()` then).
   */
  login(wc: WebContents | null, authInfo: { isProxy: boolean; host: string; port: number; realm?: string }, cb: (user?: string, pass?: string) => void): boolean {
    if (wc) {
      const e = this.byContents.get(wc);
      if (e && e.view.webContents === wc && isOurProxyChallenge(authInfo, e.part.proxy.port)) {
        cb(e.username, e.password);
        return true;
      }
    }
    const part = [...this.partitions.values()].find((x) => isOurProxyChallenge(authInfo, x.proxy.port));
    if (!part) return false;
    // A webContents of another session never gets a preview credential.
    if (wc && !this.sessions.has(wc.session)) return false;
    cb(part.worker.username, part.worker.password);
    return true;
  }

  closeAll(): void {
    for (const e of [...this.views.values()]) this.destroy(e);
    this.offHosts?.();
    this.offHosts = null;
    for (const part of this.partitions.values()) part.proxy.close();
    this.partitions.clear();
  }

  // ---- internals -------------------------------------------------------------------------------

  private entry(owner: WebContents, view: string): Entry {
    const e = this.views.get(view);
    if (!e || e.owner !== owner || e.view.webContents.isDestroyed()) throw Object.assign(new Error('unknown preview view'), { code: 'not_found' });
    return e;
  }

  private sessionFor(route: PreviewRoute, port: number): Promise<Session> {
    const partition = previewPartition(route.hostId, route.machine);
    let p = this.configured.get(partition);
    if (!p) {
      p = (async () => {
        const ses = electronSession.fromPartition(partition);
        // Before any view exists, so `web-contents-created` already sees a preview session.
        this.sessions.add(ses);
        await ses.setProxy({ mode: 'fixed_servers', proxyRules: `http://127.0.0.1:${port}`, proxyBypassRules: '<-loopback>' });
        ses.setPermissionRequestHandler((_wc, permission, cb) => cb(permission === 'clipboard-sanitized-write'));
        ses.setPermissionCheckHandler((_wc, permission) => permission === 'clipboard-sanitized-write');
        ses.setDevicePermissionHandler(() => false);
        ses.setCertificateVerifyProc((req, cb) => cb(isLoopbackName(req.hostname) ? 0 : -3));
        return ses;
      })();
      this.configured.set(partition, p);
      p.catch(() => this.configured.delete(partition));
    }
    return p;
  }

  private configureContents(e: Entry): void {
    const wc = e.view.webContents;
    wc.setWebRTCIPHandlingPolicy('disable_non_proxied_udp');
    wc.setWindowOpenHandler(({ url, disposition }) => {
      const u = safeWebUrl(url);
      if (!u) {
        this.d.log('preview: blocked window.open');
        return { action: 'deny' };
      }
      // Links meant for a new tab leave the app unless they point at the preview machine.
      if ((disposition === 'foreground-tab' || disposition === 'background-tab') && !isLoopbackUrl(u)) void shell.openExternal(u).catch(() => {});
      else void wc.loadURL(u).catch(() => {});
      return { action: 'deny' };
    });
    const guard = (ev: Electron.Event, url: string) => {
      if (!safeWebUrl(url) && !url.startsWith('about:')) {
        ev.preventDefault();
        this.d.log('preview: blocked navigation');
      }
    };
    wc.on('will-navigate', guard);
    wc.on('will-redirect', guard);
    wc.on('will-frame-navigate', (ev) => guard(ev, ev.url));
    wc.on('will-attach-webview', (ev) => ev.preventDefault());
    const changed = () => this.emit(e);
    wc.on('did-start-loading', changed);
    wc.on('did-stop-loading', changed);
    wc.on('did-navigate', (_ev, url) => {
      e.lastUrl = url;
      if (e.error && e.error !== 'host_offline') e.error = undefined;
      changed();
    });
    wc.on('did-navigate-in-page', (_ev, url, isMainFrame) => {
      if (isMainFrame) e.lastUrl = url;
      changed();
    });
    wc.on('page-title-updated', changed);
    wc.on('did-fail-load', (_ev, code, desc, url, isMainFrame) => {
      // -3 = aborted (a new navigation replaced it).
      if (!isMainFrame || code === -3) return;
      if (url) e.lastUrl = url;
      e.error = desc || `load failed (${code})`;
      changed();
    });
    wc.on('render-process-gone', () => {
      e.error = 'crashed';
      changed();
    });
    wc.on('destroyed', () => this.forget(e));
  }

  private watchOwner(owner: WebContents, win: BrowserWindow): void {
    const closeOwned = () => {
      for (const e of [...this.views.values()]) if (e.owner === owner) this.destroy(e);
    };
    if (!this.watchedOwners.has(owner)) {
      this.watchedOwners.add(owner);
      owner.on('destroyed', closeOwned);
      // A reload or full navigation of the app page drops its views (a new document opens its own).
      owner.on('did-start-navigation', (ev) => {
        if (ev.isMainFrame && !ev.isSameDocument) closeOwned();
      });
    }
    if (!this.watchedWindows.has(win)) {
      this.watchedWindows.add(win);
      win.on('closed', () => {
        for (const e of [...this.views.values()]) if (e.win === win) this.destroy(e);
      });
    }
  }

  private hostsChanged(): void {
    for (const e of this.views.values()) {
      const online = this.d.online(e.route.hostId);
      if (!online && !e.hostDown) {
        e.hostDown = true;
        e.error = 'host_offline';
        e.view.setVisible(false);
        this.emit(e);
      } else if (online && e.hostDown) {
        e.hostDown = false;
        e.error = undefined;
        e.view.setVisible(e.wantVisible);
        void e.view.webContents.loadURL(e.view.webContents.getURL() || e.lastUrl).catch(() => {});
        this.emit(e);
      }
    }
  }

  private emit(e: Entry): void {
    if (e.owner.isDestroyed() || e.view.webContents.isDestroyed()) return;
    const wc = e.view.webContents;
    const state: PreviewState = {
      view: e.id,
      url: wc.getURL() || e.lastUrl,
      title: wc.getTitle(),
      loading: wc.isLoading(),
      canGoBack: wc.navigationHistory.canGoBack(),
      canGoForward: wc.navigationHistory.canGoForward(),
      ...(e.error ? { error: e.error } : {}),
      ...(e.blocked ? { blocked: { ...e.blocked } } : {}),
    };
    e.owner.send(EVENT.previewState, state);
  }

  private destroy(e: Entry): void {
    this.forget(e);
    try {
      if (!e.win.isDestroyed()) e.win.contentView.removeChildView(e.view);
    } catch {
      /* window going away */
    }
    const wc = e.view.webContents;
    if (!wc.isDestroyed()) {
      if (wc.isDevToolsOpened()) wc.closeDevTools();
      wc.close();
    }
  }

  private forget(e: Entry): void {
    if (this.views.get(e.id) !== e) return;
    this.views.delete(e.id);
    e.part.proxy.removeView(e.id);
    if (![...this.views.values()].some((x) => x.part === e.part)) e.part.proxy.dropPool(e.route);
    if (!this.views.size) {
      this.offHosts?.();
      this.offHosts = null;
    }
  }
}
