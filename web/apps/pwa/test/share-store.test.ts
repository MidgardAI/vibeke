import { describe, expect, test } from 'bun:test';
import { MAX_SHARED_FILES, MAX_SHARED_FILE_BYTES, SHARE_TTL_MS, isExpired, isSharedRecord, newShareId, recordFromForm } from '../src/share-store';

describe('share target store', () => {
  test('ids are random hex', () => {
    const id = newShareId((n) => new Uint8Array(n).fill(171));
    expect(id).toBe('ab'.repeat(12));
  });
  test('form data becomes a record; empty, huge and surplus files are dropped', () => {
    const fd = new FormData();
    fd.set('title', 'T');
    fd.set('text', 'body');
    fd.set('url', 'https://example.com');
    fd.append('files', new File(['hi'], 'a.txt', { type: 'text/plain' }));
    fd.append('files', new File([], 'empty.txt'));
    fd.append('files', new File([new Uint8Array(MAX_SHARED_FILE_BYTES + 1)], 'big.bin'));
    for (let i = 0; i < MAX_SHARED_FILES + 3; i++) fd.append('files', new File(['x'], `f${i}.txt`));
    const r = recordFromForm(fd, 'id1', 5);
    expect(r).toMatchObject({ id: 'id1', at: 5, title: 'T', text: 'body', url: 'https://example.com' });
    expect(r.files.length).toBe(MAX_SHARED_FILES);
    expect(r.files[0]).toMatchObject({ name: 'a.txt', type: 'text/plain' });
    expect(isSharedRecord(r)).toBe(true);
    expect(isSharedRecord({ id: 1 })).toBe(false);
  });
  test('records expire after an hour', () => {
    expect(isExpired({ at: 0 }, SHARE_TTL_MS - 1)).toBe(false);
    expect(isExpired({ at: 0 }, SHARE_TTL_MS)).toBe(true);
    expect(isExpired({ at: 10 * SHARE_TTL_MS }, 0)).toBe(true);
  });
});
