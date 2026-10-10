// Content shared into the app (Web Share Target): turn it into prompt text.

import type { SharedItem } from '../platform';

/** Title, text and link on separate lines; a title that the text already repeats is dropped. */
export function sharedText(s: Pick<SharedItem, 'title' | 'text' | 'url'>): string {
  const title = s.title.trim();
  const text = s.text.trim();
  const url = s.url.trim();
  const parts: string[] = [];
  if (title && !text.includes(title) && !url.includes(title)) parts.push(title);
  if (text) parts.push(text);
  // Many apps put the link inside `text` as well.
  if (url && !text.includes(url)) parts.push(url);
  return parts.join('\n');
}

/** Append shared text to an existing draft on a new line. */
export function appendToDraft(draft: string, add: string): string {
  if (!add) return draft;
  if (!draft.trim()) return add;
  return `${draft.replace(/\s+$/, '')}\n${add}`;
}
