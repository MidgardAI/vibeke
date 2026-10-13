// Adding the injected script to HTML responses (spec 20 §3.6). The tag goes right after the
// doctype (before it would switch the page to quirks mode), else at the very start, so it runs
// before any of the page's own scripts. Pure functions: the service worker and the tests use them.

/** The script tag, carrying the page's non-HttpOnly cookies for the `document.cookie` shim. */
export function injectTag(cookies: string): string {
  const esc = cookies.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  return `<script src="/__vk/inject.js" data-vk-cookies="${esc}"></script>`;
}

/** Bytes to look at before deciding where the tag goes. */
export const SNIFF_BYTES = 1024;

const lower = (b: number): number => (b >= 65 && b <= 90 ? b + 32 : b);

/** Where the tag goes in the first bytes of a document: after the doctype, else after a BOM. */
export function insertAt(head: Uint8Array): number {
  let i = 0;
  if (head[0] === 0xef && head[1] === 0xbb && head[2] === 0xbf) i = 3;
  const start = i;
  while (i < head.length && (head[i] === 0x20 || head[i] === 0x09 || head[i] === 0x0a || head[i] === 0x0d)) i++;
  const doctype = '<!doctype';
  for (let j = 0; j < doctype.length; j++) {
    if (i + j >= head.length || lower(head[i + j]!) !== doctype.charCodeAt(j)) return start;
  }
  for (let k = i + doctype.length; k < head.length; k++) if (head[k] === 0x3e /* > */) return k + 1;
  return start;
}

/** The first bytes of a document with the tag inserted. */
export function injectInto(head: Uint8Array, tag: string): Uint8Array {
  const at = insertAt(head);
  const t = new TextEncoder().encode(tag);
  const out = new Uint8Array(head.length + t.length);
  out.set(head.subarray(0, at), 0);
  out.set(t, at);
  out.set(head.subarray(at), at + t.length);
  return out;
}

/**
 * A body stream with the tag inserted: buffers up to `SNIFF_BYTES` (or the whole body if shorter)
 * before passing anything on.
 */
export function injectStream(body: ReadableStream<Uint8Array>, tag: string): ReadableStream<Uint8Array> {
  const reader = body.getReader();
  let head: Uint8Array | null = new Uint8Array(0);
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      for (;;) {
        const { done, value } = await reader.read();
        if (head !== null) {
          if (value) {
            const merged = new Uint8Array(head.length + value.length);
            merged.set(head, 0);
            merged.set(value, head.length);
            head = merged;
          }
          if (head.length < SNIFF_BYTES && !done) continue;
          controller.enqueue(injectInto(head, tag));
          head = null;
          if (done) controller.close();
          return;
        }
        if (done) return controller.close();
        if (value) controller.enqueue(value);
        return;
      }
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });
}

/** The page shown when the review lost its connection (no controller channel within 10 s). */
export function lostPage(): string {
  return `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Review disconnected</title><body style="font:16px system-ui;margin:2rem;color:#333"><h1 style="font-size:1.25rem">This review lost its connection</h1><p>Reload the review page to connect again.</p></body>`;
}
