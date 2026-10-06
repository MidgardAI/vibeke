import { describe, expect, test } from 'bun:test';
import { fileKind } from '../src/components/file-icon';
import { fmtCount, relTime } from '../src/lib/format';

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

describe('relTime', () => {
  const now = 1_800_000_000_000;
  test('under a minute (and clock skew) is now', () => {
    expect(relTime(now, now)).toBe('now');
    expect(relTime(now - 59_000, now)).toBe('now');
    expect(relTime(now + 5 * MIN, now)).toBe('now');
    expect(relTime(Number.NaN, now)).toBe('now');
  });
  test('minutes, hours, days', () => {
    expect(relTime(now - MIN, now)).toBe('1m');
    expect(relTime(now - 12 * MIN - 30_000, now)).toBe('12m');
    expect(relTime(now - 59 * MIN, now)).toBe('59m');
    expect(relTime(now - HOUR, now)).toBe('1h');
    expect(relTime(now - 23 * HOUR, now)).toBe('23h');
    expect(relTime(now - DAY, now)).toBe('1d');
    expect(relTime(now - 3 * DAY, now)).toBe('3d');
    expect(relTime(now - 13 * DAY, now)).toBe('13d');
  });
  test('weeks, months, years', () => {
    expect(relTime(now - 14 * DAY, now)).toBe('2w');
    expect(relTime(now - 59 * DAY, now)).toBe('8w');
    expect(relTime(now - 60 * DAY, now)).toBe('2mo');
    expect(relTime(now - 364 * DAY, now)).toBe('12mo');
    expect(relTime(now - 800 * DAY, now)).toBe('2y');
  });
});

describe('fmtCount', () => {
  test('small numbers verbatim', () => {
    expect(fmtCount(0)).toBe('0');
    expect(fmtCount(684)).toBe('684');
    expect(fmtCount(999)).toBe('999');
    expect(fmtCount(-12)).toBe('12');
    expect(fmtCount(3.7)).toBe('3');
  });
  test('thousands with one decimal below 10k, never rounding up', () => {
    expect(fmtCount(1000)).toBe('1k');
    expect(fmtCount(1049)).toBe('1k');
    expect(fmtCount(1900)).toBe('1.9k');
    expect(fmtCount(1999)).toBe('1.9k');
    expect(fmtCount(9999)).toBe('9.9k');
    expect(fmtCount(12_345)).toBe('12k');
    expect(fmtCount(999_999)).toBe('999k');
  });
  test('millions', () => {
    expect(fmtCount(1_000_000)).toBe('1M');
    expect(fmtCount(1_250_000)).toBe('1.2M');
    expect(fmtCount(42_000_000)).toBe('42M');
  });
});

describe('file icons', () => {
  test('by extension, basename and folder', () => {
    expect(fileKind('src/app.tsx')).toBe('tsx');
    expect(fileKind('a/b/model.ts')).toBe('ts');
    expect(fileKind('main.rs')).toBe('rs');
    expect(fileKind('README.md')).toBe('md');
    expect(fileKind('Cargo.lock')).toBe('lock');
    expect(fileKind('web/bun.lock')).toBe('lock');
    expect(fileKind('pnpm-lock.yaml')).toBe('lock');
    expect(fileKind('ci.yml')).toBe('yaml');
    expect(fileKind('logo.PNG')).toBe('image');
    expect(fileKind('Makefile')).toBe('sh');
    expect(fileKind('.gitignore')).toBe('default');
    expect(fileKind('LICENSE')).toBe('default');
    expect(fileKind('src', true)).toBe('folder');
  });
});
