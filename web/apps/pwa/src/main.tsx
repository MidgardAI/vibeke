// PWA bootstrap: platform + service worker registration + <VibekeApp/>. No feature logic here
// (spec 16 §9.3).

import '@vibeke/ui/styles.css';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { registerSW } from 'virtual:pwa-register';
import { VibekeApp } from '@vibeke/ui';
import { createPwaPlatform } from './platform';

registerSW({ immediate: true });

const platform = createPwaPlatform();
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <VibekeApp platform={platform} />
  </StrictMode>,
);
