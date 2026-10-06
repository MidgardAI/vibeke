// Which kind of window this UI renders in: the full app, the menu-bar quick-approvals popover,
// or a popped-out pane window (spec 16 §16.2). Screens adapt (no back button in a pane window…).

import { createContext, useContext } from 'react';
import type { Surface } from './keyboard';

export const SurfaceContext = createContext<Surface>('full');
export const useSurface = (): Surface => useContext(SurfaceContext);
