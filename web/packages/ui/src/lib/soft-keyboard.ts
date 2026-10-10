// Is the on-screen keyboard open? A touch device whose visual viewport is much shorter than the
// window while a text field has focus (pinch zoom also shrinks the viewport: it is excluded).

export interface KeyboardInput {
  innerHeight: number;
  vvHeight: number;
  /** The tallest window height seen without a keyboard (browsers that resize the window for it). */
  baseHeight?: number;
  vvScale?: number;
  /** A text field (input, textarea, editable element) has focus. */
  editableFocused: boolean;
  /** The primary pointer is a finger. */
  coarse: boolean;
}

/** A viewport this much shorter than the window means a keyboard (the smallest is about 200 px). */
export const KEYBOARD_MIN_PX = 120;

export function keyboardOpen(i: KeyboardInput): boolean {
  if (!i.coarse || !i.editableFocused) return false;
  if ((i.vvScale ?? 1) > 1.05) return false;
  return Math.max(i.innerHeight, i.baseHeight ?? 0) - i.vvHeight > KEYBOARD_MIN_PX;
}
