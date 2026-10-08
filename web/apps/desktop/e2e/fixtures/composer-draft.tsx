import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { AppContext } from '../../../../packages/ui/src/app/hooks';
import type { AppModel } from '../../../../packages/ui/src/app/model';
import { useComposerDraft } from '../../../../packages/ui/src/lib/composer-draft';

const saves: string[] = [];
let stored = 'restored conversation';
const app = { platform: { drafts: {
  get: async () => stored,
  set: async (_host: string, _pane: string, text: string) => { stored = text; saves.push(text); },
} }, toast: () => {} } as unknown as AppModel;
function Composer() {
  const [conversation, setConversation] = useState(true);
  const [text, setText] = useComposerDraft('test-host', 'test-pane', conversation);
  return <section data-testid="draft-harness">
    <button onClick={() => setConversation((v) => !v)}>Toggle composer view</button>
    <input aria-label="Draft test input" value={text} onChange={(e) => setText(e.target.value)} />
    <output data-testid="draft-mode">{conversation ? 'conversation' : 'terminal'}</output>
    <output data-testid="draft-saves">{JSON.stringify(saves)}</output>
  </section>;
}
const host = document.createElement('div');
document.body.append(host);
createRoot(host).render(<AppContext.Provider value={app}><Composer /></AppContext.Provider>);
