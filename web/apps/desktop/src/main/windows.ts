// Windows (spec 16 §16.1–16.2): the main window (hidden, not destroyed, on close so the app keeps
// running in the menu bar), the frameless quick-approvals popover under the tray icon, and
// popped-out pane windows. Bounds are persisted per window.

import { BrowserWindow, nativeTheme, screen, type BrowserWindowConstructorOptions, type Rectangle, type WebContents } from 'electron';
import { EVENT } from '../shared/contract';
import { popoverPosition, readJson, restoreBounds, writeJson, type Bounds } from './store';

export type SurfaceName = 'full' | 'quick' | 'pane';

export interface WindowsOptions {
  /** URL of the renderer for a surface (+ optional hash route). */
  url(surface: SurfaceName, hash?: string): string;
  preload: string;
  stateFile: string;
  /** Called before a hidden window is shown so it can catch up on state first. */
  beforeShow(win: BrowserWindow): void;
  trayBounds(): Rectangle | null;
  onVisibilityChange(): void;
  devTools: boolean;
  /** Destroy the quick popover after it has been hidden this long (it is recreated on demand). */
  quickTtlMs: number;
  /** Destroy the hidden main window's renderer after this long (menu-bar only; recreated on show). */
  mainTtlMs: number;
}

const QUICK_SIZE = { width: 400, height: 580 };
const isMac = process.platform === 'darwin';

const bg = (): string => (nativeTheme.shouldUseDarkColors ? '#0f1012' : '#f6f6f4');

interface SavedState {
  main?: Bounds;
  panes?: Record<string, Bounds>;
}

export class Windows {
  main: BrowserWindow | null = null;
  quick: BrowserWindow | null = null;
  readonly panes = new Map<string, BrowserWindow>();
  quitting = false;
  private state: SavedState;
  private saveTimer: ReturnType<typeof setTimeout> | null = null;
  private quickHiddenAt = 0;
  private quickTimer: ReturnType<typeof setTimeout> | null = null;
  private mainTimer: ReturnType<typeof setTimeout> | null = null;
  /** Renderers that acknowledged `vk:ready` since their last (re)load; others get navigation queued. */
  private ready = new WeakSet<WebContents>();
  private queuedNav = new WeakMap<WebContents, string>();

  constructor(private readonly o: WindowsOptions) {
    this.state = readJson<SavedState>(o.stateFile, {});
  }

  private webPreferences(): BrowserWindowConstructorOptions['webPreferences'] {
    return {
      preload: this.o.preload,
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      nodeIntegrationInWorker: false,
      webSecurity: true,
      allowRunningInsecureContent: false,
      webviewTag: false,
      spellcheck: true,
      devTools: this.o.devTools,
      backgroundThrottling: true,
    };
  }

  /** Readiness is per document: a reload or full navigation waits for a new `vk:ready`. */
  private watchReady(win: BrowserWindow): void {
    const wc = win.webContents;
    // Navigation *start* (before the new document can run and acknowledge), main frame only.
    wc.on('did-start-navigation', (e) => {
      if (e.isMainFrame && !e.isSameDocument) this.ready.delete(wc);
    });
  }

  /** The renderer registered its listeners: deliver the navigation queued meanwhile. */
  markReady(wc: WebContents): void {
    this.ready.add(wc);
    const hash = this.queuedNav.get(wc);
    this.queuedNav.delete(wc);
    if (hash && !wc.isDestroyed()) wc.send(EVENT.nav, hash);
  }

  all(): BrowserWindow[] {
    return [this.main, this.quick, ...this.panes.values()].filter((w): w is BrowserWindow => !!w && !w.isDestroyed());
  }

  isOurs(wc: Electron.WebContents): BrowserWindow | null {
    return this.all().find((w) => w.webContents === wc) ?? null;
  }

  /** The user is looking at the app (notifications are pointless then). */
  appFocused(): boolean {
    return this.all().some((w) => w.isFocused() && w.isVisible());
  }

  anyVisible(): boolean {
    return this.all().some((w) => w.isVisible() && !w.isMinimized());
  }

  private displays() {
    return screen.getAllDisplays().map((d) => d.workArea);
  }

  private persist(key: 'main' | `pane:${string}`, win: BrowserWindow): void {
    if (win.isDestroyed() || win.isFullScreen()) return;
    const b: Bounds = { ...win.getNormalBounds(), maximized: win.isMaximized() };
    if (key === 'main') this.state.main = b;
    else this.state.panes = { ...this.state.panes, [key.slice(5)]: b };
    if (this.saveTimer) clearTimeout(this.saveTimer);
    this.saveTimer = setTimeout(() => {
      try {
        writeJson(this.o.stateFile, this.state);
      } catch {
        // best effort
      }
    }, 400);
  }

  private track(key: 'main' | `pane:${string}`, win: BrowserWindow): void {
    for (const ev of ['resize', 'move', 'maximize', 'unmaximize'] as const) win.on(ev as 'resize', () => this.persist(key, win));
    for (const ev of ['show', 'hide', 'minimize', 'restore', 'focus', 'blur'] as const) win.on(ev as 'show', () => this.o.onVisibilityChange());
  }

  // ---- main window ---------------------------------------------------------------------------

  createMain(show: boolean): BrowserWindow {
    if (this.main && !this.main.isDestroyed()) return this.main;
    const min = { width: 380, height: 480 };
    const saved = restoreBounds(this.state.main, this.displays(), min);
    const win = new BrowserWindow({
      width: saved?.width ?? 1120,
      height: saved?.height ?? 760,
      ...(saved ? { x: saved.x, y: saved.y } : { center: true }),
      minWidth: min.width,
      minHeight: min.height,
      show: false,
      title: 'Vibeke',
      backgroundColor: isMac ? '#00000000' : bg(),
      ...(isMac
        ? { titleBarStyle: 'hiddenInset' as const, trafficLightPosition: { x: 16, y: 16 }, vibrancy: 'sidebar' as const, visualEffectState: 'followWindow' as const, transparent: false }
        : { autoHideMenuBar: false }),
      webPreferences: this.webPreferences(),
    });
    this.main = win;
    this.track('main', win);
    this.watchReady(win);
    // `hide` is not reliably emitted for programmatic hides; `hideMain` arms this too.
    win.on('hide', () => this.armMainTtl());
    win.on('show', () => this.clearMainTtl());
    if (saved?.maximized) win.maximize();
    win.on('close', (e) => {
      // Closing keeps the app (and its connections) running in the menu bar / tray.
      if (!this.quitting) {
        e.preventDefault();
        if (win.isFullScreen()) {
          win.once('leave-full-screen', () => this.hideMain(win));
          win.setFullScreen(false);
        } else this.hideMain(win);
      }
    });
    win.on('closed', () => {
      this.clearMainTtl();
      if (this.main === win) this.main = null;
    });
    if (show) win.once('ready-to-show', () => win.show());
    void win.loadURL(this.o.url('full'));
    return win;
  }

  showMain(hash?: string): void {
    const fresh = !this.main || this.main.isDestroyed();
    const win = this.createMain(false);
    const reveal = () => {
      this.o.beforeShow(win);
      if (win.isMinimized()) win.restore();
      win.show();
      win.focus();
      this.clearMainTtl();
      this.o.onVisibilityChange();
    };
    if (hash) this.navigate(win, hash);
    if (fresh || win.webContents.isLoading()) win.once('ready-to-show', reveal);
    else reveal();
  }

  /**
   * Route a window to a hash. Until its renderer acknowledged `vk:ready` (listeners registered),
   * the latest route waits here, so a deep link opened during startup is never dropped.
   */
  navigate(win: BrowserWindow, hash: string): void {
    const wc = win.webContents;
    if (this.ready.has(wc)) wc.send(EVENT.nav, hash);
    else this.queuedNav.set(wc, hash);
  }

  /** Hide the main window: renderers learn they are hidden, and its renderer is freed later. */
  private hideMain(win: BrowserWindow): void {
    win.hide();
    this.armMainTtl();
    this.o.onVisibilityChange();
  }

  private armMainTtl(): void {
    this.clearMainTtl();
    if (this.quitting || !(this.o.mainTtlMs > 0)) return;
    this.mainTimer = setTimeout(() => {
      this.mainTimer = null;
      const w = this.main;
      if (!w || w.isDestroyed() || w.isVisible()) return;
      // Hidden for a while (menu bar only): free the renderer; showMain recreates it.
      this.main = null;
      w.destroy();
      this.o.onVisibilityChange();
    }, this.o.mainTtlMs);
  }

  private clearMainTtl(): void {
    if (this.mainTimer) clearTimeout(this.mainTimer);
    this.mainTimer = null;
  }

  // ---- quick-approvals popover ---------------------------------------------------------------

  private createQuick(): BrowserWindow {
    if (this.quick && !this.quick.isDestroyed()) return this.quick;
    const win = new BrowserWindow({
      ...QUICK_SIZE,
      show: false,
      frame: false,
      resizable: false,
      movable: false,
      minimizable: false,
      maximizable: false,
      fullscreenable: false,
      skipTaskbar: true,
      alwaysOnTop: true,
      hasShadow: true,
      roundedCorners: true,
      title: 'Vibeke quick approvals',
      backgroundColor: isMac ? '#00000000' : bg(),
      ...(isMac ? { vibrancy: 'popover' as const, visualEffectState: 'active' as const } : {}),
      webPreferences: this.webPreferences(),
    });
    win.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
    win.on('blur', () => {
      // Clicking elsewhere dismisses it, like a native menu-bar popover.
      if (!win.webContents.isDevToolsOpened()) this.hideQuick();
    });
    win.on('closed', () => {
      if (this.quickTimer) clearTimeout(this.quickTimer);
      this.quickTimer = null;
      if (this.quick === win) this.quick = null;
    });
    win.on('show', () => this.o.onVisibilityChange());
    win.on('hide', () => this.o.onVisibilityChange());
    this.watchReady(win);
    void win.loadURL(this.o.url('quick'));
    this.quick = win;
    return win;
  }

  hideQuick(): void {
    const win = this.quick;
    if (!win?.isVisible()) return;
    win.hide();
    this.quickHiddenAt = Date.now();
    this.o.onVisibilityChange();
    // Not reopened soon: free its renderer (it is recreated on demand in a few hundred ms).
    if (this.quickTimer) clearTimeout(this.quickTimer);
    this.quickTimer = setTimeout(() => {
      this.quickTimer = null;
      if (this.quick === win && !win.isDestroyed() && !win.isVisible()) win.destroy();
    }, this.o.quickTtlMs);
  }

  /** Toggle from the tray icon / global shortcut. */
  toggleQuick(): void {
    // A tray click first blurs (hides) the popover; don't immediately reopen it.
    if (this.quick?.isVisible()) return this.hideQuick();
    if (Date.now() - this.quickHiddenAt < 250) return;
    this.showQuick();
  }

  showQuick(hash?: string): void {
    if (this.quickTimer) clearTimeout(this.quickTimer);
    this.quickTimer = null;
    const win = this.createQuick();
    const display = screen.getDisplayNearestPoint(screen.getCursorScreenPoint());
    const pos = popoverPosition(this.o.trayBounds(), QUICK_SIZE, display.workArea, process.platform);
    win.setBounds({ ...pos, ...QUICK_SIZE });
    if (hash) this.navigate(win, hash);
    const reveal = () => {
      this.o.beforeShow(win);
      win.show();
      win.focus();
      this.o.onVisibilityChange();
    };
    if (win.webContents.isLoading()) win.webContents.once('did-finish-load', reveal);
    else reveal();
  }

  // ---- pane windows --------------------------------------------------------------------------

  popOutPane(host: string, pane: string): void {
    const key = `${host}/${pane}`;
    const existing = this.panes.get(key);
    if (existing && !existing.isDestroyed()) {
      existing.show();
      existing.focus();
      return;
    }
    const min = { width: 360, height: 420 };
    const saved = restoreBounds(this.state.panes?.[key], this.displays(), min);
    const win = new BrowserWindow({
      width: saved?.width ?? 760,
      height: saved?.height ?? 640,
      ...(saved ? { x: saved.x, y: saved.y } : {}),
      minWidth: min.width,
      minHeight: min.height,
      show: false,
      title: 'Vibeke',
      backgroundColor: bg(),
      ...(isMac ? { titleBarStyle: 'hiddenInset' as const, trafficLightPosition: { x: 16, y: 16 } } : {}),
      webPreferences: this.webPreferences(),
    });
    this.panes.set(key, win);
    this.track(`pane:${key}`, win);
    this.watchReady(win);
    win.on('closed', () => {
      if (this.panes.get(key) === win) this.panes.delete(key);
      this.o.onVisibilityChange();
    });
    win.once('ready-to-show', () => {
      this.o.beforeShow(win);
      win.show();
      this.o.onVisibilityChange();
    });
    void win.loadURL(this.o.url('pane', `#/h/${encodeURIComponent(host)}/p/${encodeURIComponent(pane)}`));
  }

  /** Follow a theme change in windows that paint their own background. */
  repaint(): void {
    // Vibrancy windows (macOS main + popover) are transparent; the rest paint the theme colour.
    const opaque = isMac ? [...this.panes.values()] : this.all();
    for (const w of opaque) if (!w.isDestroyed()) w.setBackgroundColor(bg());
  }
}
