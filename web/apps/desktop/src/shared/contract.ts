// The IPC contract between the renderer (preload bridge) and the main process (spec 16 §16.1).
// Kept small and explicit: every channel here has a validator in main/validate.ts.

import type { AppMethod, HostRecord, HostState } from '@vibeke/core';

/** Renderer → main (ipcRenderer.invoke / ipcMain.handle). */
export const INVOKE = {
  boot: 'vk:boot',
  engineStart: 'vk:engine.start',
  request: 'vk:host.request',
  refresh: 'vk:host.refresh',
  reconnect: 'vk:host.reconnect',
  remove: 'vk:host.remove',
  pair: 'vk:pair',
  localConnect: 'vk:local.connect',
  localStartGateway: 'vk:local.start-gateway',
  openExternal: 'vk:shell.open-external',
  clipboardWrite: 'vk:clipboard.write',
  clipboardRead: 'vk:clipboard.read',
  settingsGet: 'vk:settings.get',
  settingsSet: 'vk:settings.set',
  window: 'vk:window',
  theme: 'vk:theme',
  /** Native file picker in main; the renderer never supplies a path. */
  chooseVibeke: 'vk:local.choose-binary',
  /** Forget the chosen executable (back to automatic discovery). */
  resetVibeke: 'vk:local.reset-binary',
  /** Native folder picker (`defaultPath?`: absolute); resolves to the chosen folder or null. */
  pickDirectory: 'vk:dialog.pick-directory',
  /** The renderer's listeners are registered: main may now send it navigation. */
  ready: 'vk:ready',
  /** Start / stop receiving `EVENT.hostEvent` for one host (`hostId`, `on: boolean`). */
  hostEvents: 'vk:host.events',
} as const;

/** Main → renderer (webContents.send). */
export const EVENT = {
  hosts: 'vk:hosts',
  pairPending: 'vk:pair.pending',
  nav: 'vk:nav',
  command: 'vk:command',
  settings: 'vk:settings',
  visibility: 'vk:visibility',
  /** A host's live event (`HostEventPayload`, see host-events.ts), to windows that subscribed. */
  hostEvent: 'vk:host-event',
} as const;

/**
 * App API methods the renderer may call through the main process's connections. Connection
 * management (`hello`, `client.visibility`, `events.subscribe`) and Web Push registration stay with
 * the engine.
 */
export const RENDERER_METHODS = [
  'dashboard.get',
  'pane.read',
  'pane.send_text',
  'pane.send_keys',
  'pane.rename',
  'pane.close',
  'pane.focus',
  'agent.prompt',
  'agent.interrupt',
  'agent.transcript',
  'agent.start',
  'agent.harnesses',
  'tab.create',
  'tab.rename',
  'tab.close',
  'tab.focus',
  'preview.open',
  'interaction.list',
  'interaction.get',
  'interaction.answer',
  'interaction.answer_batch',
  'git.status',
  'git.diff',
  'git.log',
  'fs.list',
  'fs.read',
  'worktree.list',
  'fs.browse',
  'repo.candidates',
  'attachment.put',
  'notification.list',
  'notification.read',
  'prefs.get',
  'prefs.set',
  'push.unsubscribe',
  'stt.transcribe',
  'devices.list',
  'devices.revoke',
  'ping',
  'share.create',
  'handoff.export',
  'handoff.read',
  'handoff.discard',
  'handoff.begin',
  'handoff.write',
  'handoff.finish',
  'peer.invite',
  'peer.redeem',
  'peer.list',
  'peer.remove',
  'share.list',
  'share.revoke',
  'handoff.send',
  'handoff.jobs',
  'handoff.cancel',
  'handoff.peers',
] as const satisfies readonly AppMethod[];

export type RendererMethod = (typeof RENDERER_METHODS)[number];

/** A host-state update: the order of all hosts plus the states that changed. */
export interface HostsPatch {
  /** Monotonic per app run; a renderer drops patches not newer than its snapshot. */
  version: number;
  order: string[];
  changed: HostState[];
}

/** Errors cross IPC as data so the renderer can rebuild the classes `classifyError` expects. */
export type WireError =
  | { type: 'rpc'; method: string; code: number; message: string; data?: Record<string, unknown> }
  | { type: 'unknown'; method: string; mutating: boolean; opId?: string; reason: 'timeout' | 'closed' }
  | { type: 'not_connected'; hostId: string }
  | { type: 'pairing'; code: string; message: string; channelCode?: string; closeCode?: number }
  | { type: 'error'; message: string; code?: string };

export type WireResult<T> = { ok: true; value: T } | { ok: false; error: WireError };

/** Static facts the renderer needs before it renders (one fast round trip). */
export interface BootInfo {
  platform: string;
  platformName: string;
  deviceName: string;
  version: string;
  /** System accent colour `#rrggbb`, when the OS has one. */
  accent: string | null;
  settings: DesktopSettings;
  /** A local gateway socket exists (offer "Connect to this Mac" prominently). */
  localGateway: boolean;
}

export interface EngineHello {
  fingerprint: string;
  hosts: HostState[];
  /** Patch version this snapshot reflects. */
  version: number;
}

/** Desktop-only settings, persisted by main (not secret). */
export interface DesktopSettings {
  /** Electron accelerator for the quick-approvals popover; '' disables it. */
  shortcut: string;
  openAtLogin: boolean;
  /** macOS: show the Dock icon (off = menu bar only). */
  showDock: boolean;
  /**
   * The `vibeke` CLI chosen in the native picker and validated by main ('' = automatic discovery).
   * Read-only for renderers: only `chooseVibeke` / `resetVibeke` change it. (The update feed is not
   * a setting at all: it comes from the packaged app-update.yml.)
   */
  vibekePath: string;
  notifications: boolean;
}

/** The settings a renderer may change through `settingsSet`. */
export type RendererSettingsPatch = Partial<Pick<DesktopSettings, 'shortcut' | 'openAtLogin' | 'showDock' | 'notifications'>>;

export const DEFAULT_SETTINGS: DesktopSettings = {
  shortcut: 'Alt+CommandOrControl+V',
  openAtLogin: false,
  showDock: true,
  vibekePath: '',
  notifications: true,
};

export type LocalConnectResult =
  | { ok: true; record: HostRecord }
  | {
      ok: false;
      /**
       * `cli_untrusted`: found only outside the usual install locations (confirm it in the picker);
       * `cli_invalid`: the chosen/configured executable no longer passes the checks.
       */
      code: 'cli_not_found' | 'cli_untrusted' | 'cli_invalid' | 'cli_failed' | 'gateway_not_running' | 'pair_failed' | 'bad_output';
      message: string;
      detail?: string;
    };

/** Result of the native "choose vibeke" picker. */
export type ChooseVibekeResult = { ok: true; path: string } | { ok: false; canceled: true } | { ok: false; canceled: false; message: string };

export type WindowOp =
  | { op: 'pop-out'; host: string; pane: string }
  | { op: 'open-main'; hash: string }
  | { op: 'close' }
  | { op: 'quick' };

export type Theme = 'system' | 'light' | 'dark';

/** What the preload exposes as `window.vibeke` (see preload/index.ts). */
export interface Bridge {
  invoke(channel: (typeof INVOKE)[keyof typeof INVOKE], ...args: unknown[]): Promise<unknown>;
  on(channel: (typeof EVENT)[keyof typeof EVENT], cb: (payload: unknown) => void): () => void;
}
