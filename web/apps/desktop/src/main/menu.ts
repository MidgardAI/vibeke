// Native application menu (spec 16 §16.2) with accelerators. Commands go to the focused window's
// UI (palette, tabs, find…) through the `vk:command` event.

import { Menu, app, type MenuItemConstructorOptions } from 'electron';
import type { UiCommand } from '@vibeke/ui';

export interface MenuActions {
  command(cmd: UiCommand): void;
  openMain(hash?: string): void;
  toggleQuick(): void;
  connectLocal(): void;
  checkUpdates(): void;
  shortcut(): string;
  devTools: boolean;
}

export function buildMenu(a: MenuActions): Menu {
  const mac = process.platform === 'darwin';
  const cmd = (c: UiCommand) => () => a.command(c);
  const appMenu: MenuItemConstructorOptions[] = mac
    ? [
        {
          label: app.name,
          submenu: [
            { role: 'about' },
            { label: 'Check for Updates…', click: a.checkUpdates },
            { type: 'separator' },
            { label: 'Settings…', accelerator: 'Command+,', click: () => a.openMain('#/settings') },
            { label: 'Connect to This Mac…', click: () => a.connectLocal() },
            { type: 'separator' },
            { role: 'services' },
            { type: 'separator' },
            { role: 'hide' },
            { role: 'hideOthers' },
            { role: 'unhide' },
            { type: 'separator' },
            { role: 'quit' },
          ],
        },
      ]
    : [];
  const template: MenuItemConstructorOptions[] = [
    ...appMenu,
    {
      label: 'File',
      submenu: [
        { label: 'New Agent…', accelerator: 'CommandOrControl+N', click: cmd('new-agent') },
        { label: 'Pair a Host…', accelerator: 'CommandOrControl+Shift+P', click: () => a.openMain('#/pair') },
        ...(mac ? [] : [{ label: 'Connect to This Computer…', click: () => a.connectLocal() } as MenuItemConstructorOptions]),
        { type: 'separator' },
        { label: 'Open Pane in New Window', accelerator: 'CommandOrControl+Shift+O', click: cmd('pop-out') },
        { type: 'separator' },
        mac ? { role: 'close' } : { label: 'Settings', accelerator: 'Control+,', click: () => a.openMain('#/settings') },
        ...(mac ? [] : [{ type: 'separator' } as MenuItemConstructorOptions, { role: 'quit' } as MenuItemConstructorOptions]),
      ],
    },
    { role: 'editMenu' },
    {
      label: 'View',
      submenu: [
        { label: 'Command Palette…', accelerator: 'CommandOrControl+K', click: cmd('palette') },
        { label: 'Find', accelerator: 'CommandOrControl+F', click: cmd('find') },
        { type: 'separator' },
        { label: 'Inbox', accelerator: 'CommandOrControl+1', click: cmd('inbox') },
        { label: 'First Workspace', accelerator: 'CommandOrControl+2', click: cmd('workspace') },
        { label: 'Toggle Changes Panel', accelerator: 'CommandOrControl+3', click: cmd('panel') },
        { label: 'Settings', accelerator: 'CommandOrControl+4', click: cmd('settings') },
        { label: 'Show Changes', accelerator: 'CommandOrControl+Shift+E', click: cmd('changes') },
        { label: 'Toggle Sidebar', accelerator: 'CommandOrControl+\\', click: cmd('sidebar') },
        { label: 'Agent: Conversation / Terminal', accelerator: 'CommandOrControl+Shift+T', click: cmd('agent-view') },
        { type: 'separator' },
        { label: 'Agents and Approvals', accelerator: a.shortcut() || undefined, registerAccelerator: false, click: () => a.toggleQuick() },
        { type: 'separator' },
        { role: 'resetZoom' },
        { role: 'zoomIn' },
        { role: 'zoomOut' },
        { type: 'separator' },
        { role: 'togglefullscreen' },
        ...(a.devTools ? [{ type: 'separator' } as MenuItemConstructorOptions, { role: 'reload' } as MenuItemConstructorOptions, { role: 'toggleDevTools' } as MenuItemConstructorOptions] : []),
      ],
    },
    {
      label: 'Go',
      submenu: [{ label: 'Back', accelerator: mac ? 'Command+[' : 'Alt+Left', click: cmd('back') }],
    },
    { role: 'windowMenu' },
    {
      role: 'help',
      submenu: [
        ...(!mac ? [{ label: 'Check for Updates…', click: a.checkUpdates }] : []),
        { label: 'Keyboard Shortcuts', accelerator: 'CommandOrControl+/', click: cmd('shortcuts') },
      ],
    },
  ];
  return Menu.buildFromTemplate(template);
}
