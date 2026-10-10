// The one modal primitive (palette, sheets, confirmations, tour, lock). Rendered into its own
// layer under <body>; while open, everything else (the app and any dialog below it) is `inert`, so
// neither Tab nor a click can reach the approval buttons underneath. Tab cycles inside, Escape
// closes the top dialog only, and focus returns to the control that opened it.

import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode, type RefObject } from 'react';
import { createPortal } from 'react-dom';

/** Open dialogs, bottom to top. Only the top one is interactive. */
const stack: HTMLElement[] = [];
/** Elements we made inert (so we only ever undo our own). */
const inerted = new Set<Element>();

function syncInert(): void {
  const top = stack[stack.length - 1] ?? null;
  for (const el of [...inerted]) {
    if (top && el !== top && el.isConnected) continue;
    el.removeAttribute('inert');
    inerted.delete(el);
  }
  if (!top) return;
  for (const el of Array.from(document.body.children)) {
    if (el === top || el.tagName === 'SCRIPT' || el.hasAttribute('inert')) continue;
    el.setAttribute('inert', '');
    inerted.add(el);
  }
}

const FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"]), [contenteditable="true"]';

function focusables(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.getClientRects().length > 0 && !el.closest('[inert]'));
}

/** True while any modal dialog is open (keyboard layers stand down). */
export const dialogOpen = (): boolean => stack.length > 0;

export interface DialogProps {
  open: boolean;
  onClose(): void;
  /** Accessible name: a string label, or the id of a visible title (see `useDialogTitleId`). */
  label?: string;
  labelledBy?: string;
  /** Element to focus first (else the first focusable control, else the panel). */
  initialFocus?: RefObject<HTMLElement | null>;
  /** Classes for the full-screen layer (positioning of the panel, scrim colour). */
  className?: string;
  /** Classes for the panel (the element with role="dialog"). */
  panelClassName?: string;
  /** Escape / scrim click close it (the lock screen does not). */
  dismissable?: boolean;
  /** `alertdialog` for confirmations. */
  role?: 'dialog' | 'alertdialog';
  /** Bottom sheet on narrow screens: a downward drag on the panel dismisses it. */
  dragDismiss?: boolean;
  /**
   * Close when the route changes (browser or edge-swipe Back, a link). Dialogs never add history
   * entries, so Back always moves one level up in the app and takes the dialog with it.
   */
  closeOnNavigate?: boolean;
  children: ReactNode;
}

export function Dialog({ open, ...rest }: DialogProps) {
  if (!open || typeof document === 'undefined') return null;
  return <DialogLayer {...rest} />;
}

function DialogLayer({ onClose, label, labelledBy, initialFocus, className, panelClassName, dismissable = true, role = 'dialog', dragDismiss, closeOnNavigate, children }: Omit<DialogProps, 'open'>) {
  const [host] = useState(() => {
    const el = document.createElement('div');
    el.className = 'vk-layer';
    return el;
  });
  const panel = useRef<HTMLDivElement>(null);
  const scrim = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  close.current = onClose;

  useEffect(() => {
    if (!closeOnNavigate || !dismissable) return;
    const path = () => window.location.hash.split('?')[0];
    const opened = path();
    const on = () => {
      if (path() !== opened) close.current();
    };
    window.addEventListener('hashchange', on);
    return () => window.removeEventListener('hashchange', on);
  }, [closeOnNavigate, dismissable]);

  useEffect(() => {
    if (!dragDismiss || !dismissable || !panel.current) return;
    return attachSheetDrag(panel.current, scrim.current, () => close.current());
  }, [dragDismiss, dismissable]);

  // Mount the layer, make the rest inert, move focus in; undo all of it (and restore focus) on close.
  useLayoutEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    document.body.appendChild(host);
    stack.push(host);
    syncInert();
    const p = panel.current;
    // Children mount into the layer before it is attached, so React `autoFocus` cannot work on
    // open: mark the control with `data-autofocus` instead. Focus already inside is kept.
    if (!(p && p.contains(document.activeElement))) {
      const first = initialFocus?.current ?? (p ? (p.querySelector<HTMLElement>('[data-autofocus]') ?? focusables(p)[0]) : null) ?? p;
      first?.focus({ preventScroll: true });
    }
    return () => {
      const i = stack.indexOf(host);
      if (i >= 0) stack.splice(i, 1);
      host.remove();
      syncInert();
      // Back to the invoking control when it still exists and nothing else took focus.
      const active = document.activeElement;
      if (opener?.isConnected && !opener.closest('[inert]') && (!active || active === document.body || !active.isConnected)) opener.focus({ preventScroll: true });
    };
  }, [host]);

  const onKeyDown = (e: KeyboardEvent) => {
    if (stack[stack.length - 1] !== host) return;
    if (e.key === 'Escape' && dismissable) {
      e.preventDefault();
      e.stopPropagation();
      close.current();
      return;
    }
    if (e.key !== 'Tab' || !panel.current) return;
    const f = focusables(panel.current);
    if (!f.length) {
      e.preventDefault();
      panel.current.focus();
      return;
    }
    const first = f[0]!;
    const last = f[f.length - 1]!;
    if (e.shiftKey && (document.activeElement === first || document.activeElement === panel.current)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  };

  return createPortal(
    <div className={className ?? 'fixed inset-0 z-50 flex items-center justify-center p-4'} onKeyDown={onKeyDown}>
      <div ref={scrim} aria-hidden className="vk-scrim absolute inset-0" onClick={dismissable ? () => close.current() : undefined} />
      <div ref={panel} role={role} aria-modal="true" aria-label={labelledBy ? undefined : label} aria-labelledby={labelledBy} tabIndex={-1} className={`relative ${panelClassName ?? ''}`}>
        {children}
      </div>
    </div>,
    host,
  );
}

/** A drag must pass this many px, or this fraction of the sheet, or be this fast (px/ms). */
const DRAG_MIN_PX = 140;
const DRAG_FRACTION = 0.3;
const DRAG_FLICK = 0.5;

/**
 * Finger-tracked dismissal of a bottom sheet. It starts only on a vertical downward drag when
 * nothing inside is scrolled down, so it does not fight inner scrolling, and only below the `sm`
 * breakpoint where the panel is a bottom sheet. Reduced motion: no tracking animation, the sheet
 * closes or stays at once.
 */
function attachSheetDrag(panel: HTMLElement, scrim: HTMLElement | null, close: () => void): () => void {
  const reduced = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false;
  const sheet = () => window.matchMedia?.('(max-width: 639px)').matches ?? false;
  let g: { x: number; y: number; dy: number; engaged: boolean; dead: boolean; v: number; lastY: number; lastT: number } | null = null;

  const scrolledDown = (from: EventTarget | null): boolean => {
    for (let n = from as HTMLElement | null; n; n = n.parentElement) {
      if (n.scrollTop > 0) return true;
      if (n === panel) break;
    }
    return false;
  };
  const apply = (dy: number) => {
    panel.style.transform = dy > 0 ? `translateY(${dy}px)` : '';
    if (scrim) scrim.style.opacity = dy > 0 ? String(1 - Math.min(1, dy / Math.max(1, panel.offsetHeight)) * 0.7) : '';
  };
  const reset = () => {
    panel.style.transition = '';
    panel.style.transform = '';
    if (scrim) {
      scrim.style.transition = '';
      scrim.style.opacity = '';
    }
  };

  const start = (e: TouchEvent) => {
    const target = e.target as HTMLElement | null;
    g = null;
    if (e.touches.length !== 1 || !sheet() || target?.closest('input,textarea,select,[contenteditable="true"]')) return;
    const t0 = e.touches[0]!;
    g = { x: t0.clientX, y: t0.clientY, dy: 0, engaged: false, dead: scrolledDown(target), v: 0, lastY: t0.clientY, lastT: e.timeStamp };
  };
  const move = (e: TouchEvent) => {
    if (!g || g.dead) return;
    if (e.touches.length !== 1) {
      g.dead = true;
      if (g.engaged) {
        g.engaged = false;
        reset();
      }
      return;
    }
    const t0 = e.touches[0]!;
    const dy = t0.clientY - g.y;
    const dx = t0.clientX - g.x;
    if (!g.engaged) {
      if (dy < -6 || (Math.abs(dx) > 8 && Math.abs(dx) > Math.abs(dy))) {
        g.dead = true;
        return;
      }
      if (dy <= 8) return;
      g.engaged = true;
      panel.style.transition = 'none';
      if (scrim) scrim.style.transition = 'none';
    }
    if (e.cancelable) e.preventDefault();
    const dt = e.timeStamp - g.lastT;
    if (dt > 0) g.v = (t0.clientY - g.lastY) / dt;
    g.lastY = t0.clientY;
    g.lastT = e.timeStamp;
    g.dy = Math.max(0, dy);
    apply(g.dy);
  };
  const end = () => {
    const s = g;
    g = null;
    if (!s || !s.engaged) return;
    const h = panel.offsetHeight;
    const go = s.dy > Math.min(DRAG_MIN_PX, h * DRAG_FRACTION) || (s.v > DRAG_FLICK && s.dy > 30);
    if (reduced()) {
      if (go) close();
      else reset();
      return;
    }
    panel.style.transition = 'transform 0.18s cubic-bezier(0.2, 0.8, 0.2, 1)';
    if (scrim) scrim.style.transition = 'opacity 0.18s';
    if (go) {
      apply(h);
      if (scrim) scrim.style.opacity = '0';
      setTimeout(close, 180);
    } else {
      apply(0);
      if (scrim) scrim.style.opacity = '';
      setTimeout(reset, 200);
    }
  };

  panel.addEventListener('touchstart', start, { passive: true });
  panel.addEventListener('touchmove', move, { passive: false });
  panel.addEventListener('touchend', end);
  panel.addEventListener('touchcancel', end);
  return () => {
    panel.removeEventListener('touchstart', start);
    panel.removeEventListener('touchmove', move);
    panel.removeEventListener('touchend', end);
    panel.removeEventListener('touchcancel', end);
  };
}

/** An id for a dialog's visible title (pass it as `labelledBy`). */
export const useDialogTitleId = (): string => useId();
