import { expect, test } from 'bun:test';
import { parsePrefs } from '../src/lib/prefs';
import { parseRoute, formatRoute } from '../src/router';
test('terminal preferences survive storage and reject malformed values', () => {
  const settings = { theme: 'light', termFont: 18, tuiScreenReader: true, tuiOptionMeta: true, tuiHintDismissed: true, preferredTuiHost: 'host' };
  expect(parsePrefs(JSON.stringify(settings))).toMatchObject(settings);
  expect(parsePrefs('{"tuiScreenReader":"true","termFont":200,"preferredTuiHost":44}')).toMatchObject({ tuiScreenReader: false, termFont: 12, preferredTuiHost: null });
});
test('terminal bookmarks preserve encoded host, workspace and pane', () => {
  const route = { name: 'tui' as const, host: 'h/1', workspace: 'work & one', pane: 'p+2' };
  expect(parseRoute(formatRoute(route))).toEqual(route);
});
