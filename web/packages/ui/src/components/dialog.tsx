// The one modal primitive (palette, sheets, confirmations, tour, lock). Rendered into its own
// layer under <body>; while open, everything else (the app and any dialog below it) is `inert`, so
// neither Tab nor a click can reach the approval buttons underneath. Tab cycles inside, Escape
// closes the top dialog only, and focus returns to the control that opened it.

import { useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode, type RefObject } from 'react';
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
  children: ReactNode;
}

export function Dialog({ open, ...rest }: DialogProps) {
  if (!open || typeof document === 'undefined') return null;
  return <DialogLayer {...rest} />;
}

function DialogLayer({ onClose, label, labelledBy, initialFocus, className, panelClassName, dismissable = true, role = 'dialog', children }: Omit<DialogProps, 'open'>) {
  const [host] = useState(() => {
    const el = document.createElement('div');
    el.className = 'vk-layer';
    return el;
  });
  const panel = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  close.current = onClose;

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
      <div aria-hidden className="vk-scrim absolute inset-0" onClick={dismissable ? () => close.current() : undefined} />
      <div ref={panel} role={role} aria-modal="true" aria-label={labelledBy ? undefined : label} aria-labelledby={labelledBy} tabIndex={-1} className={`relative ${panelClassName ?? ''}`}>
        {children}
      </div>
    </div>,
    host,
  );
}

/** An id for a dialog's visible title (pass it as `labelledBy`). */
export const useDialogTitleId = (): string => useId();
