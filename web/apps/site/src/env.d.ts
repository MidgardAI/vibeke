/// <reference types="vite/client" />
declare module 'virtual:vibeke-docs' {
  export const apiMethods: { name: string; params: string; result: string; mutating: boolean; pane_scope: string }[]
  export const docs: import('../content/manifest').Doc[]
}
