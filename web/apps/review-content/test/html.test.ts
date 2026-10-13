import { describe, expect, test } from 'bun:test';
import { injectInto, injectStream, injectTag, insertAt } from '../src/html';

const te = new TextEncoder();
const td = new TextDecoder();

describe('script injection', () => {
  const tag = injectTag('a=1; b="x"<');
  test('the tag escapes the cookies', () => {
    expect(tag).toBe('<script src="/__vk/inject.js" data-vk-cookies="a=1; b=&quot;x&quot;&lt;"></script>');
  });
  test('after the doctype, never before it', () => {
    expect(td.decode(injectInto(te.encode('<!DOCTYPE html><html>'), '<s>'))).toBe('<!DOCTYPE html><s><html>');
    expect(td.decode(injectInto(te.encode('\n  <!doctype html>\n<p>'), '<s>'))).toBe('\n  <!doctype html><s>\n<p>');
    expect(td.decode(injectInto(te.encode('<html><head>'), '<s>'))).toBe('<s><html><head>');
    const bom = new Uint8Array([0xef, 0xbb, 0xbf, ...te.encode('<p>')]);
    expect(insertAt(bom)).toBe(3);
    expect(insertAt(te.encode('<!doctype html'))).toBe(0);
  });
  test('stream: small chunks are buffered, the rest passes', async () => {
    const parts = ['<!doc', 'type html>', '<body>', 'x'.repeat(2000), 'end'];
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        for (const p of parts) c.enqueue(te.encode(p));
        c.close();
      },
    });
    const out = await new Response(injectStream(body, '<s>')).text();
    expect(out).toBe(`<!doctype html><s><body>${'x'.repeat(2000)}end`);
    const short = new ReadableStream<Uint8Array>({
      start(c) {
        c.enqueue(te.encode('<p>hi'));
        c.close();
      },
    });
    expect(await new Response(injectStream(short, '<s>')).text()).toBe('<s><p>hi');
  });
});
