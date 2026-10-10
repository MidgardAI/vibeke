// PWA bootstrap: platform + service worker registration + <VibekeApp/>. No feature logic here
// (spec 16 §9.3).

import '@vibeke/ui/styles.css';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { registerSW } from 'virtual:pwa-register';
import { VibekeApp } from '@vibeke/ui';
import { createAppUpdate } from './app-update';
import { createPwaPlatform } from './platform';

const CHECK_MS = 60 * 60 * 1000;

// A new service worker waits until the user taps the update prompt (no silent skipWaiting).
const update = createAppUpdate(() => void updateSW(true));
const updateSW = registerSW({
  immediate: true,
  onNeedRefresh: () => update.markWaiting(),
  onRegisteredSW(_url, reg) {
    if (!reg) return;
    // Look for a new version hourly and whenever the app comes back to the foreground.
    const check = () => void reg.update().catch(() => {});
    setInterval(check, CHECK_MS);
    document.addEventListener('visibilitychange', () => document.visibilityState === 'visible' && check());
  },
});

const platform = createPwaPlatform(update);
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <VibekeApp platform={platform} />
  </StrictMode>,
);
