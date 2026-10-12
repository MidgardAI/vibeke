import { describe, expect, test } from 'bun:test';
import { PICK_ELEMENT_FN, elementPick, pickClip, pickPointExpression, pickRegionExpression, regionPick } from '../src/annotate-js';

describe('annotate page scripts', () => {
  test('the page functions are valid JavaScript', () => {
    expect(() => new Function(`return (${PICK_ELEMENT_FN});`)).not.toThrow();
    expect(() => new Function(`return ${pickPointExpression(10.5, 20)};`)).not.toThrow();
    expect(() => new Function(`return ${pickRegionExpression({ x: 1, y: 2, w: 300, h: 40 })};`)).not.toThrow();
  });

  test('expressions carry only numbers from the caller', () => {
    expect(pickPointExpression(Number.NaN, 5)).toContain('})(0, 5)');
    expect(pickRegionExpression({ x: 1.234, y: 2, w: 3, h: 4 })).toContain('})(1.23, 2, 3, 4)');
  });

  test('picks map to annotation_add fields', () => {
    const rect = { x: 1, y: 2, w: 3, h: 4 };
    expect(elementPick({ selector: '#a', role: 'button', name: 'Go', text: 'Go', html: '<b>', rect })).toEqual({ kind: 'element', selector: '#a', role: 'button', name: 'Go', text: 'Go', html: '<b>', rect });
    const covers = Array.from({ length: 14 }, (_, i) => ({ selector: `li:nth-of-type(${i + 1})` }));
    const r = regionPick(rect, { container: 'ul', covers });
    expect(r.kind).toBe('region');
    expect(r.covers).toHaveLength(10);
    expect(r.selector).toBe('ul');
  });

  test('the clip adds a margin, stays on the page and is capped', () => {
    expect(pickClip({ x: 100, y: 50, w: 200, h: 100 })).toEqual({ x: 92, y: 42, width: 216, height: 116, scale: 1 });
    expect(pickClip({ x: 2, y: 3, w: 10, h: 10 })).toEqual({ x: 0, y: 0, width: 20, height: 21, scale: 1 });
    const big = pickClip({ x: 0, y: 0, w: 5000, h: 4000 });
    expect(big.width).toBe(1600);
    expect(big.height).toBe(1600);
  });
});
