import { describe, expect, test } from 'bun:test';
import { REDACTED, localSocketPath, redact } from '../src';

describe('redact (mirrors vk-redact)', () => {
  test('known token shapes', () => {
    expect(redact('key sk-ant-api03-abcdefgh and ghp_' + 'a'.repeat(36))).toBe(`key ${REDACTED} and ${REDACTED}`);
    expect(redact('AKIAABCDEFGHIJKLMNOP')).toBe(REDACTED);
    expect(redact('eyJhbGciOi.eyJzdWIiOi.sig')).toBe(REDACTED);
  });
  test('prefixes and assignments keep their context', () => {
    expect(redact('curl -H "Authorization: Bearer abcdefghijkl"')).toBe(`curl -H "Authorization: ${REDACTED}"`);
    expect(redact('git clone https://user:hunter2@example.com/x')).toBe(`git clone https://${REDACTED}@example.com/x`);
    expect(redact('PASSWORD=hunter2 api_key: "abc def" token=\'x\'')).toBe(`PASSWORD=${REDACTED} api_key: "${REDACTED}" token='${REDACTED}'`);
  });
  test('PEM blocks, even truncated', () => {
    expect(redact('-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY----- tail')).toBe(`${REDACTED} tail`);
    expect(redact('x -----BEGIN PRIVATE KEY-----\nMIIE')).toBe(`x ${REDACTED}`);
  });
  test('ordinary text is unchanged', () => {
    expect(redact('pnpm test --filter web')).toBe('pnpm test --filter web');
  });
  test('local socket paths', () => {
    expect(localSocketPath('local:/a b/gateway.sock')).toBe('/a b/gateway.sock');
    expect(localSocketPath('local:rel')).toBeNull();
    expect(localSocketPath('wss://x')).toBeNull();
  });
});
