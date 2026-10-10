// Is the on-screen keyboard open on this touch device? (`visualViewport` shrinks while a text
// field has focus; see lib/soft-keyboard.ts.) Never saved: it follows the keyboard.

import { useEffect, useState } from 'react';
import { keyboardOpen } from './soft-keyboard';

const NON_TEXT = new Set(['button', 'checkbox', 'radio', 'submit', 'reset', 'range', 'file', 'color', 'image']);

function editableFocused(): boolean {
  const a = document.activeElement as HTMLElement | null;
  if (!a) return false;
  if (a.tagName === 'TEXTAREA' || a.isContentEditable) return true;
  return a.tagName === 'INPUT' && !NON_TEXT.has((a as HTMLInputElement).type);
}

export function useSoftKeyboard(): boolean {
  const [open, setOpen] = useState(false);
  useEffect(() => {
    const vv = typeof window !== 'undefined' ? window.visualViewport : null;
    if (!vv) return;
    const coarse = window.matchMedia?.('(pointer: coarse)');
    let base = window.innerHeight;
    let landscape = window.innerWidth > window.innerHeight;
    const update = () => {
      const land = window.innerWidth > window.innerHeight;
      if (land !== landscape) {
        landscape = land;
        base = window.innerHeight;
      }
      const editable = editableFocused();
      if (!editable) base = Math.max(base, window.innerHeight);
      setOpen(keyboardOpen({ innerHeight: window.innerHeight, vvHeight: vv.height, vvScale: vv.scale, editableFocused: editable, coarse: !!coarse?.matches, baseHeight: base }));
    };
    // The focused element settles after the event: look again on the next tick.
    const later = () => setTimeout(update, 50);
    vv.addEventListener('resize', update);
    window.addEventListener('focusin', later);
    window.addEventListener('focusout', later);
    window.addEventListener('orientationchange', later);
    update();
    return () => {
      vv.removeEventListener('resize', update);
      window.removeEventListener('focusin', later);
      window.removeEventListener('focusout', later);
      window.removeEventListener('orientationchange', later);
    };
  }, []);
  return open;
}
