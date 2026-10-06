// Right panel entry points. The components load lazily so the panel stays out of the main chunk:
// `lazy(() => import('./right-panel'))` (the layout) and `lazy(() => import('./centre-diff'))`
// (the workspace screen, while `showsCentreDiff(route)`).

export { showsCentreDiff } from './routes';
