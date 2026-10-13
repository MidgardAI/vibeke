// The page functions of core `annotate-js.ts`, compiled into the injected script by the
// `virtual:vk-pick` plugin in vite.config.ts (no eval in the page).
declare module 'virtual:vk-pick' {
  export const pickElement: (this: Element) => {
    selector: string;
    role: string;
    name: string;
    text: string;
    html: string;
    rect: { x: number; y: number; w: number; h: number };
  } | null;
  export const pickRegion: (rx: number, ry: number, rw: number, rh: number) => { container: string; covers: { selector: string; text?: string }[] };
}
