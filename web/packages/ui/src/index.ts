// @vibeke/ui: every screen and component of the Vibeke apps (spec 16 §9.3). Shells provide a
// UiPlatform and mount <VibekeApp/>.
export { VibekeApp } from './app/app';
export { AppModel } from './app/model';
export { useApp, useHosts, useAllHosts, useInboxItems, usePrefs } from './app/hooks';
export { emitUi, isMacLike, type Surface } from './app/keyboard';
export type {
  UiPlatform,
  NotificationsCapability,
  InstallCapability,
  SpeechCapability,
  MirrorCache,
  BuildInfo,
  HapticKind,
  PermissionState,
  HostEngine,
  WindowsCapability,
  UiExtensions,
  ShellCommand,
  UiCommand,
} from './platform';
export { parseRoute, formatRoute, hashFromUrl, navigate, type Route } from './router';
export { hostOfTag, staleTags, badgeCount, openCounts } from './lib/notify';
export { shortcutFor, keyLabel, fuzzyScore, SHORTCUTS, type ShortcutAction, type KeyLike, type KeyContext } from './lib/shortcuts';
export { listNav } from './lib/list-nav';
export type { KV, Prefs, Theme } from './lib/prefs';
export { pairingErrorMessage } from './screens/pair';
// Primitives, for shell-provided extensions (UiExtensions) to match the app.
export { Button, Card, Notice, SectionLabel, Segmented, Spinner, TextField, Toggle, Dot, cx } from './components/ui';
export { t, en, type Strings } from './i18n';
