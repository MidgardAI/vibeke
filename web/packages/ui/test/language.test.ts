import { describe, expect, test } from 'bun:test';
import { en, loadStrings, resolveLanguage } from '../src/i18n';
import { parsePrefs } from '../src/lib/prefs';

describe('language', () => {
  test('system follows the browser, falling back to English', () => {
    expect(resolveLanguage('system', ['en-GB', 'nb'])).toBe('en');
    expect(resolveLanguage('system', ['xx-YY'])).toBe('en');
    expect(resolveLanguage('system', undefined)).toBe('en');
    expect(resolveLanguage('en', 'xx')).toBe('en');
  });
  test('the dictionary loads through one function', async () => {
    expect(await loadStrings('en')).toBe(en);
  });
  test('the preference is validated', () => {
    expect(parsePrefs(null).language).toBe('system');
    expect(parsePrefs('{"language":"en"}').language).toBe('en');
    expect(parsePrefs('{"language":"zz"}').language).toBe('system');
  });
});
