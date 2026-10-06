// Desktop renderer bootstrap: one bridge round trip for boot facts, then the shared app for this
// window's surface (`?surface=full|quick|pane`). No product logic here (spec 16 §9.3).

import '@vibeke/ui/styles.css';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { AppModel, VibekeApp, navigate, type Surface } from '@vibeke/ui';
import { EVENT, INVOKE, type BootInfo, type Bridge } from '../shared/contract';
import { createDesktopPlatform } from './platform';

declare global {
  interface Window {
    vibeke: Bridge;
  }
}

const bridge = window.vibeke;
const q = new URLSearchParams(location.search).get('surface');
const surface: Surface = q === 'quick' || q === 'pane' ? q : 'full';

// Notification clicks, deep links and "open in main window" route here. Registered before
// anything else and then acknowledged: main queues navigation until it hears `vk:ready`.
bridge.on(EVENT.nav, (hash) => typeof hash === 'string' && hash.startsWith('#/') && navigate(hash));
void bridge.invoke(INVOKE.ready).catch(() => {});

async function main() {
  const boot = (await bridge.invoke(INVOKE.boot)) as BootInfo;
  const root = document.documentElement;
  root.dataset.surface = surface;
  root.dataset.platform = boot.platform;
  if (boot.platform === 'darwin') {
    // Traffic lights over the content (hiddenInset) and native vibrancy behind the sidebar/popover.
    if (surface !== 'quick') root.dataset.titlebar = 'inset';
    if (surface !== 'pane') root.dataset.vibrancy = '';
  }
  if (boot.accent) {
    root.dataset.accent = '';
    root.style.setProperty('--system-accent', boot.accent);
  }

  const platform = createDesktopPlatform(bridge, boot);
  const model = new AppModel(platform);
  // The window's theme choice drives the native appearance (vibrancy material, title bar).
  let theme = model.prefs.get().theme;
  void bridge.invoke(INVOKE.theme, theme).catch(() => {});
  model.prefs.subscribe(() => {
    const next = model.prefs.get().theme;
    if (next !== theme) void bridge.invoke(INVOKE.theme, (theme = next)).catch(() => {});
  });

  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      <VibekeApp platform={platform} model={model} surface={surface} />
    </StrictMode>,
  );
}

void main().catch((e) => {
  document.body.textContent = `Vibeke failed to start: ${(e as Error).message}`;
});
