// Page-side JavaScript for annotating a page (browser annotations). The strings are function
// declarations that run inside the page, in an isolated world, through CDP
// `Runtime.callFunctionOn` (`this` is the picked node) or `Runtime.evaluate`. They never write to
// the page and return plain JSON. Keep the limits in step with the host's pick engine.

import type { AnnotationPick, AnnotationRect } from './model';

/** Longest text kept from a picked element. */
export const PICK_TEXT_MAX = 2000;
/** Longest outerHTML kept from a picked element. */
export const PICK_HTML_MAX = 2048;
/** Most elements listed for a region pick. */
export const PICK_COVERS_MAX = 10;

/** The result of the element function in the page. */
export type ElementPick = Required<Pick<AnnotationPick, 'selector' | 'role' | 'name' | 'text' | 'html' | 'rect'>>;

// Shared helpers, declared inside each function so every string stands alone.
const HELPERS = `
  const esc = (s) => (window.CSS && CSS.escape ? CSS.escape(s) : String(s).replace(/[^\\w-]/g, '\\\\$&'));
  const unique = (sel) => { try { return document.querySelectorAll(sel).length === 1; } catch (e) { return false; } };
  const selectorOf = (el) => {
    const parts = [];
    let n = el;
    while (n && n.nodeType === 1 && n !== document.documentElement && parts.length < 8) {
      const tag = n.localName;
      if (n.id) {
        const s = '#' + esc(n.id);
        if (unique(s)) { parts.unshift(s); break; }
      }
      const tid = n.getAttribute('data-testid');
      if (tid) {
        const s = tag + '[data-testid="' + String(tid).replace(/["\\\\]/g, '\\\\$&') + '"]';
        if (unique(s)) { parts.unshift(s); break; }
      }
      let part = tag;
      const p = n.parentElement;
      if (p) {
        const same = Array.prototype.filter.call(p.children, (c) => c.localName === tag);
        if (same.length > 1) part += ':nth-of-type(' + (same.indexOf(n) + 1) + ')';
      }
      parts.unshift(part);
      n = p;
    }
    return parts.join(' > ');
  };
  const clean = (s, max) => String(s || '').replace(/\\s+/g, ' ').trim().slice(0, max);
`;

/** Implicit ARIA roles for the elements people most often pick. */
const ROLES = `{a:'link',button:'button',img:'img',input:'textbox',select:'combobox',textarea:'textbox',nav:'navigation',main:'main',header:'banner',footer:'contentinfo',h1:'heading',h2:'heading',h3:'heading',h4:'heading',h5:'heading',h6:'heading',ul:'list',ol:'list',li:'listitem',table:'table',form:'form',dialog:'dialog'}`;

/** `Runtime.callFunctionOn` declaration: describe `this` (an element). */
export const PICK_ELEMENT_FN = `function () {
  const el = this && this.nodeType === 1 ? this : this && this.parentElement;
  if (!el) return null;
  ${HELPERS}
  const roles = ${ROLES};
  let role = el.getAttribute('role') || roles[el.localName] || '';
  if (el.localName === 'input') {
    const t = (el.getAttribute('type') || 'text').toLowerCase();
    role = t === 'checkbox' ? 'checkbox' : t === 'radio' ? 'radio' : t === 'button' || t === 'submit' ? 'button' : 'textbox';
  }
  let name = el.getAttribute('aria-label') || el.getAttribute('alt') || el.getAttribute('title') || el.getAttribute('placeholder') || '';
  if (!name && el.labels && el.labels[0]) name = el.labels[0].textContent;
  if (!name) name = el.innerText || el.textContent || '';
  const r = el.getBoundingClientRect();
  return {
    selector: selectorOf(el),
    role,
    name: clean(name, 120),
    text: clean(el.innerText || el.textContent, ${PICK_TEXT_MAX}),
    html: String(el.outerHTML || '').slice(0, ${PICK_HTML_MAX}),
    rect: { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) },
  };
}`;

/**
 * `Runtime.evaluate` expression: describe the element at a viewport point (the topmost element
 * that is not the document itself), or null.
 */
export function pickPointExpression(x: number, y: number): string {
  return `(function (x, y) {
  const el = document.elementFromPoint(x, y);
  if (!el) return null;
  return (${PICK_ELEMENT_FN}).call(el);
})(${num(x)}, ${num(y)})`;
}

/**
 * `Runtime.evaluate` expression: the smallest element that contains a viewport rectangle and up
 * to ${PICK_COVERS_MAX} elements inside it, found on a grid of `elementsFromPoint` samples.
 */
export function pickRegionExpression(rect: AnnotationRect): string {
  return `(function (rx, ry, rw, rh) {
  ${HELPERS}
  const seen = new Set();
  const found = [];
  const cols = Math.max(2, Math.min(8, Math.ceil(rw / 40)));
  const rows = Math.max(2, Math.min(8, Math.ceil(rh / 40)));
  for (let i = 0; i < cols; i++) {
    for (let j = 0; j < rows; j++) {
      const px = rx + ((i + 0.5) * rw) / cols;
      const py = ry + ((j + 0.5) * rh) / rows;
      const el = document.elementFromPoint(px, py);
      if (el && !seen.has(el) && el !== document.documentElement && el !== document.body) {
        seen.add(el);
        found.push(el);
      }
    }
  }
  let container = document.body || document.documentElement;
  if (found.length) {
    container = found[0];
    while (container && container !== document.body && !found.every((f) => container.contains(f))) container = container.parentElement;
    if (!container) container = document.body;
  }
  const covers = found
    .filter((f) => f !== container)
    .slice(0, ${PICK_COVERS_MAX})
    .map((f) => ({ selector: selectorOf(f), text: clean(f.innerText || f.textContent, 120) }));
  return { container: selectorOf(container), covers };
})(${num(rect.x)}, ${num(rect.y)}, ${num(rect.w)}, ${num(rect.h)})`;
}

/** The result of the region expression in the page. */
export interface RegionPick {
  container: string;
  covers: { selector: string; text?: string }[];
}

const num = (n: number): string => (Number.isFinite(n) ? String(Math.round(n * 100) / 100) : '0');

/** An element pick as `browser.annotation_add` fields. */
export function elementPick(p: ElementPick): AnnotationPick {
  return { kind: 'element', selector: p.selector, role: p.role, name: p.name, text: p.text, html: p.html, rect: p.rect };
}

/** A region pick as `browser.annotation_add` fields: the container selector and what the region covers. */
export function regionPick(rect: AnnotationRect, r: RegionPick): AnnotationPick {
  return { kind: 'region', selector: r.container, rect, covers: r.covers.slice(0, PICK_COVERS_MAX) };
}

/** The screenshot clip for a picked rectangle: an 8 px margin, at most 1600 px a side. */
export function pickClip(rect: AnnotationRect, margin = 8, max = 1600): { x: number; y: number; width: number; height: number; scale: number } {
  const x = Math.max(0, rect.x - margin);
  const y = Math.max(0, rect.y - margin);
  const width = Math.max(1, Math.min(max, rect.x + rect.w + margin - x));
  const height = Math.max(1, Math.min(max, rect.y + rect.h + margin - y));
  return { x, y, width, height, scale: 1 };
}
