// Content received through the Web Share Target. The service worker stores it under a one-time
// id; the page reads it once (`takeShared`) and deletes it. Entries older than an hour are
// dropped. No DOM: shared by the service worker, the page and tests.

export const SHARE_TTL_MS = 60 * 60 * 1000;
export const MAX_SHARED_FILES = 10;
export const MAX_SHARED_FILE_BYTES = 8 * 1024 * 1024;
const MAX_TEXT = 20_000;

export interface SharedRecord {
  id: string;
  at: number;
  title: string;
  text: string;
  url: string;
  files: { name: string; type: string; blob: Blob }[];
}

/** A random, unguessable id (URL safe). */
export function newShareId(random: (n: number) => Uint8Array): string {
  return [...random(12)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

const field = (form: { get(name: string): unknown }, name: string): string => {
  const v = form.get(name);
  return typeof v === 'string' ? v.slice(0, MAX_TEXT) : '';
};

/** Build a record from the share POST's form data; oversized or surplus files are dropped. */
export function recordFromForm(form: { get(name: string): unknown; getAll(name: string): unknown[] }, id: string, now: number): SharedRecord {
  const files: SharedRecord['files'] = [];
  for (const f of form.getAll('files')) {
    if (typeof f === 'string' || !f || typeof (f as Blob).size !== 'number') continue;
    const file = f as Blob & { name?: string };
    if (file.size === 0 || file.size > MAX_SHARED_FILE_BYTES || files.length >= MAX_SHARED_FILES) continue;
    files.push({ name: (file.name || `shared-${files.length + 1}`).slice(0, 200), type: file.type || 'application/octet-stream', blob: file });
  }
  return { id, at: now, title: field(form, 'title'), text: field(form, 'text'), url: field(form, 'url'), files };
}

export const isExpired = (r: Pick<SharedRecord, 'at'>, now: number): boolean => !(now - r.at < SHARE_TTL_MS) || r.at > now + 60_000;

/** Type guard for a stored value. */
export function isSharedRecord(v: unknown): v is SharedRecord {
  if (!v || typeof v !== 'object') return false;
  const r = v as Partial<SharedRecord>;
  return typeof r.id === 'string' && typeof r.at === 'number' && Array.isArray(r.files);
}
