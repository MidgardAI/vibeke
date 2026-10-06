// Menu-bar / tray icon (spec 16 §16.2): template image on macOS with the open-interaction count
// as its title, the Dock badge, and a context menu. Click (or the global shortcut) toggles the
// quick-approvals popover.

import { Menu, Tray, app, nativeImage } from 'electron';

export interface TrayActions {
  toggleQuick(): void;
  openMain(hash?: string): void;
  connectLocal(): void;
  quit(): void;
  shortcut(): string;
}

export class AppTray {
  private tray: Tray | null = null;
  private count = -1;

  constructor(
    private readonly icons: { template: string; color: string },
    private readonly a: TrayActions,
  ) {}

  create(): void {
    const mac = process.platform === 'darwin';
    const img = nativeImage.createFromPath(mac ? this.icons.template : this.icons.color);
    if (mac) img.setTemplateImage(true);
    const tray = new Tray(img.isEmpty() ? nativeImage.createEmpty() : img);
    tray.setToolTip('Vibeke');
    if (mac) tray.setIgnoreDoubleClickEvents(true);
    tray.on('click', () => this.a.toggleQuick());
    tray.on('right-click', () => tray.popUpContextMenu(this.menu()));
    // Linux trays often only show a menu (no click events): give them one with the actions.
    if (process.platform === 'linux') tray.setContextMenu(this.menu());
    this.tray = tray;
    this.setCount(0);
  }

  bounds(): Electron.Rectangle | null {
    if (!this.tray || process.platform === 'linux') return null;
    const b = this.tray.getBounds();
    return b.width > 0 ? b : null;
  }

  private menu(): Menu {
    const sc = this.a.shortcut();
    return Menu.buildFromTemplate([
      { label: 'Quick Approvals', accelerator: sc || undefined, registerAccelerator: false, click: () => this.a.toggleQuick() },
      { label: 'Open Vibeke', click: () => this.a.openMain() },
      { label: 'Inbox', click: () => this.a.openMain('#/inbox') },
      { type: 'separator' },
      { label: 'Connect to This Computer…', click: () => this.a.connectLocal() },
      { label: 'Settings…', click: () => this.a.openMain('#/settings') },
      { type: 'separator' },
      { label: 'Quit Vibeke', role: 'quit', click: () => this.a.quit() },
    ]);
  }

  /** Open interactions across hosts: tray title (macOS), Dock / launcher badge. */
  setCount(n: number): void {
    if (n === this.count) return;
    this.count = n;
    const label = n > 99 ? '99+' : String(n);
    if (process.platform === 'darwin') {
      this.tray?.setTitle(n > 0 ? label : '', { fontType: 'monospacedDigit' });
      app.dock?.setBadge(n > 0 ? label : '');
    } else {
      app.setBadgeCount(n);
    }
    this.tray?.setToolTip(n > 0 ? `Vibeke — ${n} need${n === 1 ? 's' : ''} you` : 'Vibeke');
    if (process.platform === 'linux') this.tray?.setContextMenu(this.menu());
  }

  destroy(): void {
    this.tray?.destroy();
    this.tray = null;
  }
}
