/** Leave composed and layout-generated text to xterm. Translate intentional modified keys. */
function optionKey(event: KeyboardEvent): string | undefined {
  const { code, shiftKey: shift, keyCode } = event;
  // The legacy letter code is layout-aware on Mac (physical KeyQ can be A).
  // This matches xterm's macOptionIsMeta handling. Physical codes are a fallback.
  if (keyCode >= 65 && keyCode <= 90) return String.fromCharCode(keyCode + (shift ? 0 : 32));
  if (keyCode >= 48 && keyCode <= 57) return shift ? ')!@#$%^&*('[keyCode - 48] : String.fromCharCode(keyCode);
  if (/^Key[A-Z]$/.test(code)) return shift ? code.slice(3) : code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(code)) return shift ? ')!@#$%^&*('[Number(code.slice(5))] : code.slice(5);
  const punctuation: Record<string, [string, string]> = { Period: ['.', '>'], Comma: [',', '<'], Slash: ['/', '?'], Semicolon: [';', ':'], Quote: ["'", '"'], BracketLeft: ['[', '{'], BracketRight: [']', '}'], Backslash: ['\\', '|'], Minus: ['-', '_'], Equal: ['=', '+'], Backquote: ['`', '~'], Space: [' ', ' '] };
  return punctuation[code]?.[shift ? 1 : 0];
}
export function createTuiKeyboard(send: (key: string, mods: number, repeat: boolean, release: boolean) => boolean,
  options: { mac: boolean; optionAsMeta: () => boolean }) {
  const pressed = new Map<string, { key: string; mods: number }>();
  return {
    releaseAll() {
      for (const { key, mods } of pressed.values()) send(key, mods, false, true);
      pressed.clear();
    },
    handle(event: KeyboardEvent): boolean {
      const code = event.code || event.key;
      if (event.type === 'keyup') {
        const held = pressed.get(code);
        if (!held) return true;
        pressed.delete(code);
        send(held.key, held.mods, false, true);
        event.preventDefault();
        return false;
      }
      const metaKey = options.mac && event.altKey && options.optionAsMeta() ? optionKey(event) : undefined;
      if (event.isComposing || (['Process', 'Dead', 'AltGraph'].includes(event.key) && !metaKey)) return true;
      const text = [...event.key].length === 1;
      if (text && (event.getModifierState?.('AltGraph') || (!options.mac && event.ctrlKey && event.altKey))) return true;
      if (text && options.mac && event.altKey && !event.ctrlKey && !options.optionAsMeta()) return true;
      if (event.metaKey || (event.ctrlKey && event.shiftKey && ['C', 'V'].includes(event.key.toUpperCase()))) return true;
      if (!(event.ctrlKey || event.altKey || (event.shiftKey && event.key === 'Enter'))) return true;
      if (event.type === 'keypress') return false;
      const mods = (event.shiftKey ? 1 : 0) | (event.altKey ? 2 : 0) | (event.ctrlKey ? 4 : 0);
      const key = metaKey ?? event.key;
      if (!send(key, mods, event.repeat, false)) return true;
      pressed.set(code, { key, mods });
      event.preventDefault();
      return false;
    },
  };
}
