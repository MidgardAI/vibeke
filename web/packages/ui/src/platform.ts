// What a shell (PWA, Electron) must provide to the UI (spec 16 §9.3). Everything browser- or
// OS-specific sits behind this interface; screens only use these capabilities.

import type { HostStore, Platform } from '@vibeke/core';
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
}
