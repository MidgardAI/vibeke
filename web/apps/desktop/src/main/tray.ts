// Menu-bar / tray icon (spec 16 §16.2): template image on macOS, with a badge and the count of
// agents that need you as its title, the Dock badge, and a context menu. Click (or the global
// shortcut) toggles the popover with the agents and quick approvals.

import { Menu, Tray, app, nativeImage, type NativeImage } from 'electron';
import type { TraySummary } from './tray-state';

export interface TrayActions {
  toggleQuick(): void;
  openMain(hash?: string): void;
  connectLocal(): void;
  quit(): void;
  shortcut(): string;
}

export class AppTray {
  private tray: Tray | null = null;
  private last = '';
  private images: { plain: NativeImage; badge: NativeImage } | null = null;

  constructor(
    private readonly icons: { template: string; badgeTemplate: string; color: string },
    private readonly a: TrayActions,
  ) {}

  create(): void {
    const mac = process.platform === 'darwin';
    const load = (path: string) => {
      const img = nativeImage.createFromPath(path);
      if (mac) img.setTemplateImage(true);
      return img.isEmpty() ? nativeImage.createEmpty() : img;
    };
    const plain = load(mac ? this.icons.template : this.icons.color);
    // Other platforms show the full-colour logo; the launcher badge carries the count there.
    this.images = { plain, badge: mac ? load(this.icons.badgeTemplate) : plain };
    const tray = new Tray(plain);
    tray.setToolTip('Vibeke');
    if (mac) tray.setIgnoreDoubleClickEvents(true);
    tray.on('click', () => this.a.toggleQuick());
    tray.on('right-click', () => tray.popUpContextMenu(this.menu()));
    // Linux trays often only show a menu (no click events): give them one with the actions.
    if (process.platform === 'linux') tray.setContextMenu(this.menu());
    this.tray = tray;
    this.setSummary({ needs: 0, working: 0 });
  }

  bounds(): Electron.Rectangle | null {
    if (!this.tray || process.platform === 'linux') return null;
    const b = this.tray.getBounds();
    return b.width > 0 ? b : null;
  }

  private menu(): Menu {
    const sc = this.a.shortcut();
    return Menu.buildFromTemplate([
      { label: 'Agents and Approvals', accelerator: sc || undefined, registerAccelerator: false, click: () => this.a.toggleQuick() },
      { label: 'Open Vibeke', click: () => this.a.openMain() },
      { label: 'Inbox', click: () => this.a.openMain('#/inbox') },
      { type: 'separator' },
      { label: 'Connect to This Computer…', click: () => this.a.connectLocal() },
      { label: 'Settings…', click: () => this.a.openMain('#/settings') },
      { type: 'separator' },
      { label: 'Quit Vibeke', role: 'quit', click: () => this.a.quit() },
    ]);
  }

  /** Agents that need you across hosts: badge glyph and title (macOS), Dock / launcher badge. */
  setSummary({ needs, working }: TraySummary): void {
    const key = `${needs}/${working}`;
    if (key === this.last) return;
    const countChanged = this.last.split('/')[0] !== String(needs);
    this.last = key;
    const label = needs > 99 ? '99+' : String(needs);
    if (process.platform === 'darwin') {
      if (this.images) this.tray?.setImage(needs > 0 ? this.images.badge : this.images.plain);
      this.tray?.setTitle(needs > 0 ? label : '', { fontType: 'monospacedDigit' });
      app.dock?.setBadge(needs > 0 ? label : '');
    } else {
      app.setBadgeCount(needs);
    }
    const parts = [needs > 0 ? `${needs} need${needs === 1 ? 's' : ''} you` : '', working > 0 ? `${working} working` : ''].filter(Boolean);
    this.tray?.setToolTip(parts.length ? `Vibeke — ${parts.join(', ')}` : 'Vibeke');
    if (countChanged && process.platform === 'linux') this.tray?.setContextMenu(this.menu());
  }

  destroy(): void {
    this.tray?.destroy();
    this.tray = null;
  }
}
