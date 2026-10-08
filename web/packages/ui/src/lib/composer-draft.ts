import { useCallback, useEffect, useRef, useState, type SetStateAction } from 'react';
import { useApp } from '../app/hooks';

/** Desktop drafts are encrypted by main. Browsers keep the existing in-memory behaviour. */
export function useComposerDraft(host: string, pane: string) {
  const app = useApp();
  const [text, setText] = useState('');
  const value = useRef('');
  const generation = useRef(0);
  const warned = useRef(false);
  useEffect(() => {
    const token = ++generation.current;
    value.current = '';
    setText('');
    warned.current = false;
    void app.platform.drafts?.get(host, pane).then((s) => {
      if (generation.current === token) { value.current = s; setText(s); }
    }).catch(() => {});
    return () => { generation.current++; };
  }, [app, host, pane]);
  const update = useCallback((next: SetStateAction<string>) => {
    generation.current++;
    const s = typeof next === 'function' ? next(value.current) : next;
    value.current = s;
    setText(s);
    void app.platform.drafts?.set(host, pane, s).catch(() => {
      if (!warned.current) { warned.current = true; app.toast('This draft could not be saved. Keep the app open until you have copied or sent it.', 'warn'); }
    });
  }, [app, host, pane]);
  return [text, update] as const;
}
