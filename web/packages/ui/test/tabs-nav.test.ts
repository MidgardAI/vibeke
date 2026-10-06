import { describe, expect, test } from 'bun:test';
import { TAB_PANEL_ID, rovingTab, tabDomId, tabKeyTarget } from '../src/lib/tabs-nav';

const ids = ['a:p1', 't:p1', 't:p2', 'p:v1'];

describe('workspace tabs keyboard', () => {
  test('one tab in the Tab order: focused, else selected, else the first', () => {
    expect(rovingTab(ids, 't:p1', null)).toBe('t:p1');
    expect(rovingTab(ids, 't:p1', 'p:v1')).toBe('p:v1');
    // The focused tab disappeared: back to the selection.
    expect(rovingTab(ids, 't:p1', 'gone')).toBe('t:p1');
    // The selected tab disappeared too (or the centre shows a diff): the first tab.
    expect(rovingTab(ids, 'gone', null)).toBe('a:p1');
    expect(rovingTab([], 'a:p1', null)).toBeNull();
  });

  test('←/→ wrap, Home/End jump, other keys are not handled', () => {
    expect(tabKeyTarget(ids, 'a:p1', 'ArrowRight')).toBe('t:p1');
    expect(tabKeyTarget(ids, 'p:v1', 'ArrowRight')).toBe('a:p1');
    expect(tabKeyTarget(ids, 'a:p1', 'ArrowLeft')).toBe('p:v1');
    expect(tabKeyTarget(ids, 't:p2', 'ArrowLeft')).toBe('t:p1');
    expect(tabKeyTarget(ids, 't:p2', 'Home')).toBe('a:p1');
    expect(tabKeyTarget(ids, 'a:p1', 'End')).toBe('p:v1');
    expect(tabKeyTarget(ids, null, 'ArrowRight')).toBe('a:p1');
    expect(tabKeyTarget(ids, null, 'ArrowLeft')).toBe('p:v1');
    for (const k of ['Enter', ' ', 'ArrowDown', 'j', 'Tab']) expect(tabKeyTarget(ids, 'a:p1', k)).toBeNull();
    expect(tabKeyTarget([], 'a:p1', 'ArrowRight')).toBeNull();
  });

  test('tabs and the panel reference each other by valid DOM ids', () => {
    expect(tabDomId('a:p1')).toBe('ws-tab-a_p1');
    expect(tabDomId('p:prev/1 x')).toMatch(/^[A-Za-z0-9_-]+$/);
    expect(TAB_PANEL_ID).toMatch(/^[A-Za-z0-9_-]+$/);
  });
});
