import { useCallback, useEffect, useRef, useState, type SetStateAction } from 'react';
import { useApp } from '../app/hooks';

/** Keep both views' drafts in memory; only conversation text may reach desktop storage. */
export function useComposerDraft(host: string, pane: string, persist: boolean) {
  const app = useApp();
  const [text, setText] = useState('');
  const values = useRef({ conversation: '', terminal: '' });
  const conversationEdited = useRef(false);
  const conversationMode = useRef(persist);
  conversationMode.current = persist;
  const generation = useRef(0);
  const warned = useRef(false);
  useEffect(() => {
    const token = ++generation.current;
    values.current = { conversation: '', terminal: '' };
    conversationEdited.current = false;
    setText('');
    warned.current = false;
    void app.platform.drafts?.get(host, pane).then((s) => {
      if (generation.current === token && !conversationEdited.current) {
        values.current.conversation = s;
        if (conversationMode.current) setText(s);
      }
    }).catch(() => {});
    return () => { generation.current++; };
  }, [app, host, pane]);
  useEffect(() => { setText(values.current[persist ? 'conversation' : 'terminal']); }, [persist]);
  const update = useCallback((next: SetStateAction<string>) => {
    const mode = persist ? 'conversation' : 'terminal';
    const s = typeof next === 'function' ? next(values.current[mode]) : next;
    values.current[mode] = s;
    setText(s);
    if (persist) {
      conversationEdited.current = true;
      void app.platform.drafts?.set(host, pane, s).catch(() => {
        if (!warned.current) { warned.current = true; app.toast('This draft could not be saved. Keep the app open until you have copied or sent it.', 'warn'); }
      });
    }
  }, [app, host, pane, persist]);
  return [text, update] as const;
}
