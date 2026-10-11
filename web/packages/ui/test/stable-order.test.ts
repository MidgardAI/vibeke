import { describe, expect, test } from 'bun:test';
import { freezeRows, type MemberRow } from '../src/lib/stable-order';

const card = (key: string): MemberRow => ({ key, members: [key] });
const batch = (key: string, ...members: string[]): MemberRow => ({ key, members });
const keys = (rows: readonly MemberRow[]) => rows.map((r) => r.key);

describe('inbox rows hold still while frozen', () => {
  test('not frozen before the first render', () => {
    const next = [card('a'), card('b')];
    expect(freezeRows(null, next)).toEqual({ rows: next, held: 0 });
  });
  test('a risk-first re-sort does not move rows', () => {
    const prev = [card('a'), card('b'), card('c')];
    const r = freezeRows(prev, [card('c'), card('a'), card('b')]);
    expect(keys(r.rows)).toEqual(['a', 'b', 'c']);
    expect(r.held).toBe(0);
  });
  test('arrivals wait and are counted', () => {
    const r = freezeRows([card('a'), card('b')], [card('new1'), card('a'), card('new2'), card('b')]);
    expect(keys(r.rows)).toEqual(['a', 'b']);
    expect(r.held).toBe(2);
  });
  test('a card that merged into a batch keeps its place', () => {
    const r = freezeRows([card('x'), card('a'), card('y')], [batch('B', 'a', 'z'), card('x'), card('y')]);
    expect(keys(r.rows)).toEqual(['x', 'B', 'y']);
    expect(r.held).toBe(0);
  });
  test('a batch that split keeps its place and shows its cards there', () => {
    const r = freezeRows([card('x'), batch('B', 'a', 'b'), card('y')], [card('y'), card('b'), card('x'), card('a')]);
    expect(keys(r.rows)).toEqual(['x', 'a', 'b', 'y']);
    expect(r.held).toBe(0);
  });
  test('a row that left disappears', () => {
    const r = freezeRows([card('a'), card('b')], [card('b')]);
    expect(keys(r.rows)).toEqual(['b']);
  });
});
