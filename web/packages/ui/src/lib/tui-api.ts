// Bump this with incompatible JS/Rust browser interface changes.
export const BROWSER_TUI_API = 2;

export interface BrowserTui {
  connected(clientId: string, features: string): void;
  disconnected(reason: string): void;
  receive(data: Uint8Array): void;
  input(data: Uint8Array): void;
  paste(text: string): void;
  key(name: string, mods: number, repeat: boolean, release: boolean): boolean;
  resize(cols: number, rows: number, cellW: number, cellH: number, dpr: number): void;
  focus(focused: boolean): void;
  visible(visible: boolean): void;
  appearance(light: boolean): void;
  focus_target(workspace: string, pane: string): void;
  location(): string;
  action(name: string): void;
  toast(message: string): void;
  quit_reason(): string | undefined;
  tick(): number | undefined;
  dirty(): boolean;
  render(): Uint8Array;
  outgoing(): Uint8Array;
  take_url(): string | undefined;
  take_download(): [string, Uint8Array] | undefined;
  take_clipboard(): string | undefined;
  free(): void;
}
