// @vibeke/ui: every screen and component of the Vibeke apps (spec 16 §9.3). Shells provide a
// UiPlatform and mount <VibekeApp/>.
export { VibekeApp } from './app/app';
export { AppModel } from './app/model';
export type { UiPlatform, NotificationsCapability, InstallCapability, SpeechCapability, MirrorCache, BuildInfo, HapticKind, PermissionState } from './platform';
export { parseRoute, formatRoute, hashFromUrl, type Route } from './router';
export { hostOfTag, staleTags, badgeCount } from './lib/notify';
export { t, en, type Strings } from './i18n';
