// Preload (sandboxed, context-isolated): exposes `window.vibeke`, a narrow bridge with an
// allow-list of invoke channels and event channels. The renderer never sees `ipcRenderer` or
// the IPC event objects; the main process validates every call again (sender + arguments).

import { contextBridge, ipcRenderer, type IpcRendererEvent } from 'electron';
import { EVENT, INVOKE, type Bridge } from '../shared/contract';

const invokeChannels = new Set<string>(Object.values(INVOKE));
const eventChannels = new Set<string>(Object.values(EVENT));

const bridge: Bridge = {
  invoke(channel, ...args) {
    if (!invokeChannels.has(channel)) return Promise.reject(new Error(`unknown channel ${channel}`));
    return ipcRenderer.invoke(channel, ...args);
  },
  on(channel, cb) {
    if (!eventChannels.has(channel)) throw new Error(`unknown event ${channel}`);
    const f = (_e: IpcRendererEvent, payload: unknown) => cb(payload);
    ipcRenderer.on(channel, f);
    return () => {
      ipcRenderer.removeListener(channel, f);
    };
  },
};

contextBridge.exposeInMainWorld('vibeke', bridge);
