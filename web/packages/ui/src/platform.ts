// What a shell (PWA, Electron) must provide to the UI (spec 16 §9.3). Everything browser- or
// OS-specific sits behind this interface; screens only use these capabilities.

import type { ComponentType } from 'react';
import type { HostManagerApi, HostRecord, HostStore, PairingLink, Platform } from '@vibeke/core';
import type { Segment } from './lib/ansi';
import type { KV } from './lib/prefs';

export type PermissionState = 'default' | 'granted' | 'denied' | 'unsupported';
export type HapticKind = 'tap' | 'success' | 'warning' | 'error';

export interface NotificationsCapability {
  permission(): PermissionState;
  /** Must be called from a user gesture. */
  requestPermission(): Promise<PermissionState>;
  /** Tags of notifications currently shown. */
  shownTags(): Promise<string[]>;
  close(tags: string[]): Promise<void>;
  setBadge(n: number): void;
  /** A notification tap routed to the running app (`url` is a hash route). */
  onOpen(cb: (url: string) => void): () => void;
  /** True where Web Push needs a home-screen install first (iOS Safari tab). */
  needsInstallForPush(): boolean;
}

export interface InstallCapability {
  /** A `beforeinstallprompt` is held. */
  canPrompt(): boolean;
  prompt(): Promise<void>;
  subscribe(cb: () => void): () => void;
  standalone: boolean;
  /** iOS/iPadOS Safari (installs via the share sheet). */
  iosShareSheet: boolean;
}

export interface SpeechCapability {
  /** Browser recognizer (Web Speech API) available. */
  recognizer: boolean;
  /** Start live recognition; resolves with the final transcript on stop. */
  recognize?(onPartial: (text: string) => void): { stop(): Promise<string>; cancel(): void };
  /** Audio recording available (for host-side transcription). */
  recorder: boolean;
  record?(): Promise<{ stop(): Promise<{ mime: string; data: Uint8Array }>; cancel(): void }>;
}

/** A cached screen: `lines` holds styled spans (colour mirror), else `text` may carry ANSI. */
export interface CachedMirror {
  text: string;
  at: number;
  lines?: Segment[][];
}

export interface MirrorCache {
  get(key: string): Promise<CachedMirror | null>;
  set(key: string, value: CachedMirror): Promise<void>;
}

export interface BuildInfo {
  version: string;
  hash: string;
  /** The origin serving this app's code (§9.4). */
  origin: string;
}

export interface UiPlatform extends Platform {
  hostStore: HostStore;
  /** Small non-secret key-value storage (prefs, pins). */
  kv: KV;
  mirrorCache?: MirrorCache;
  haptics?(kind: HapticKind): void;
  clipboard: { writeText(text: string): Promise<void>; readText?(): Promise<string> };
  /** Open an http(s) URL outside the app. */
  openExternal(url: string): void;
  /** The OS share sheet (Web Share API), when available. Rejects if the user dismisses it. */
  share?(data: { title?: string; text?: string; url: string }): Promise<void>;
  notifications?: NotificationsCapability;
  install?: InstallCapability;
  speech?: SpeechCapability;
  build: BuildInfo;
  /** Default device name for pairing, e.g. "iPhone". */
  defaultDeviceName: string;
  /** Client identity sent in `hello`. */
  client: { client: string; version: string };
  /** Run host connections outside the UI (Electron main process). */
  engine?: HostEngine;
  /** Camera QR scanning on the pairing screen (default: when the browser supports it). */
  qrScan?: boolean;
  /** Notifications are shown by the shell itself from live connections (no Web Push). */
  localAlerts?: boolean;
  /** Can reach `local:` (Unix socket) gateways on this machine. */
  localTransport?: boolean;
  windows?: WindowsCapability;
  extensions?: UiExtensions;
  /** Commands from native menus / tray routed to this window. */
  onCommand?(cb: (cmd: UiCommand) => void): () => void;
  /** macOS-style platform (Cmd instead of Ctrl for shortcuts). Defaults to sniffing the UA. */
  mac?: boolean;
}

/**
 * Host connections run outside the UI (Electron: in the main process, so they outlive windows and
 * one connection per host serves every window). Without an engine the UI runs core's
 * `HostManager` itself with the platform's keystore and sockets (PWA).
 */
export interface HostEngine {
  /** Connect everything; resolves with the manager proxy and this device's key fingerprint. */
  start(): Promise<{ manager: HostManagerApi; fingerprint: string }>;
  /** Pair with `link`; the engine persists the host and starts its connection. */
  pair(link: PairingLink, deviceName: string, onPending: (fingerprint: string) => void): Promise<HostRecord>;
}

/** Multi-window shells (Electron). */
export interface WindowsCapability {
  /** Open a pane in its own window (terminal mirror + composer + keys). */
  popOutPane?(host: string, pane: string): void;
  /** Show the main window at a hash route (from the quick popover or a pane window). */
  openMain?(hash: string): void;
  /** Close (hide) this window: Esc in the quick popover. */
  close?(): void;
}

/** A command contributed by the shell to the command palette. */
export interface ShellCommand {
  id: string;
  title: string;
  hint?: string;
  keywords?: string;
  run(): void;
}

/** Shell-provided UI (rendered with the shared primitives exported from `@vibeke/ui`). */
export interface UiExtensions {
  /** Shown at the top of the pairing screen (desktop: "Connect to this Mac"). */
  pairPanel?: ComponentType;
  /** Extra settings group (desktop: shortcut, start at login, dock). */
  settingsSection?: ComponentType;
  /** Extra palette commands. */
  commands?(): ShellCommand[];
}

/** Named commands a shell can send into the UI (menu accelerators, tray, notifications). */
export type UiCommand =
  | 'palette'
  | 'shortcuts'
  | 'find'
  | 'new-agent'
  | 'inbox'
  | 'panes'
  | 'focus'
  | 'changes'
  | 'settings'
  | 'pair'
  | 'back'
  | 'pop-out';
