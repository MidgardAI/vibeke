// Signing in to a cloud provider (spec 17 §5). The host says how (`methods`); this renders
// whatever kinds it knows: a masked token field with a "Get a token" link, an import button,
// or a note that the host already uses an environment variable.
//
// `withCloudAuth(conn, provider, call)` runs any cloud call. When the host answers `needs_auth`
// it opens the sign-in sheet (mounted by each window as <CloudAuthHost/>), and retries the
// call once after a successful sign-in. The token goes to the host in one `cloud.auth.set` call
// and is dropped from the form at once; it is never stored or logged here.

import { useEffect, useMemo, useState } from 'react';
import { Download, ExternalLink, KeyRound } from 'lucide-react';
import { needsAuth, type CloudAuthMethod, type HostConnectionApi } from '@vibeke/core';
import { cloudStores, useHostCloud } from '../app/cloud-stores';
import { useApp } from '../app/hooks';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { CloudAuthQueue, runWithCloudAuth, type CloudAuthRequest } from '../lib/cloud';
import { Button, Notice, Sheet, TextField } from './ui';

const safeUrl = (u: string | undefined): string | null => {
  if (!u) return null;
  try {
    const p = new URL(u);
    return p.protocol === 'https:' || p.protocol === 'http:' ? p.href : null;
  } catch {
    return null;
  }
};

export interface CloudAuthFormProps {
  methods: readonly CloudAuthMethod[];
  /** The provider uses this environment variable already (from `cloud.providers`). */
  envVar?: string | null;
  busy?: boolean;
  error?: string | null;
  onToken(token: string): void;
  onImport(source: string): void;
  onHelp(url: string): void;
}

/** The generic renderer of a provider's sign-in methods. */
export function CloudAuthForm({ methods, envVar, busy, error, onToken, onImport, onHelp }: CloudAuthFormProps) {
  const [token, setToken] = useState('');
  const paste = methods.filter((m): m is Extract<CloudAuthMethod, { kind: 'paste_token' }> => m.kind === 'paste_token');
  const imports = methods.filter((m): m is Extract<CloudAuthMethod, { kind: 'import' }> => m.kind === 'import');
  const envs = methods.filter((m): m is Extract<CloudAuthMethod, { kind: 'env' }> => m.kind === 'env');
  return (
    <div className="space-y-3">
      {envVar && <Notice tone="ok">{t.cloud.envNote(envVar)}</Notice>}
      {error && <Notice tone="danger">{error}</Notice>}
      {paste.map((m, i) => {
        const help = safeUrl(m.help_url);
        return (
          <form
            key={`p${i}`}
            className="space-y-2"
            onSubmit={(e) => {
              e.preventDefault();
              const v = token.trim();
              if (!v) return;
              setToken('');
              onToken(v);
            }}
          >
            <TextField type="password" label={m.label || t.cloud.token} value={token} onChange={(e) => setToken(e.target.value)} autoComplete="off" autoCapitalize="off" spellCheck={false} disabled={busy} />
            {m.hint && <div className="text-xs text-muted">{m.hint}</div>}
            <div className="flex gap-2">
              {help && (
                <Button type="button" variant="outline" icon={<ExternalLink className="size-4" />} onClick={() => onHelp(help)}>
                  {t.cloud.getToken}
                </Button>
              )}
              <Button type="submit" variant="primary" block busy={busy} disabled={!token.trim()} icon={<KeyRound className="size-4" />}>
                {t.cloud.save}
              </Button>
            </div>
          </form>
        );
      })}
      {imports.map((m, i) => (
        <Button key={`i${i}`} block variant="outline" busy={busy} icon={<Download className="size-4" />} onClick={() => onImport(m.source)}>
          {t.cloud.importFrom(m.label)}
        </Button>
      ))}
      {envs.map((m, i) => (
        <div key={`e${i}`} className="text-xs text-muted">
          {t.cloud.envHint(m.var)}
        </div>
      ))}
    </div>
  );
}

// ---- the sign-in sheet and the retry helper ------------------------------------------------

interface AuthRequest extends CloudAuthRequest {
  conn: HostConnectionApi;
  label: string;
  methods: CloudAuthMethod[];
}

let nextId = 1;
/** The mounted <CloudAuthHost/>s of this window; the newest one shows the requests. */
const handlers: ((r: AuthRequest) => void)[] = [];

/** Sign in, outside any failed call (the Sandboxes screen's "Sign in"). Resolves true when signed in. */
export function requestCloudAuth(conn: HostConnectionApi, provider: string, methods: CloudAuthMethod[], label = provider): Promise<boolean> {
  return new Promise((resolve) => {
    const handler = handlers[handlers.length - 1];
    if (!handler) return resolve(false);
    handler({ id: nextId++, host: conn.id, conn, provider, label, methods, done: resolve });
  });
}

/**
 * Run `call`; on `needs_auth` show the sign-in UI for `provider`, then retry once. Rejects with
 * the original error when the user closes the sheet without signing in.
 */
export function withCloudAuth<T>(conn: HostConnectionApi, provider: string, call: () => Promise<T>): Promise<T> {
  return runWithCloudAuth(call, (p, methods) => requestCloudAuth(conn, p || provider, methods));
}

/**
 * Mounted wherever a cloud sheet can open (the main window and pop-out pane windows): shows the
 * sign-in sheet for one request at a time; later requests wait their turn.
 */
export function CloudAuthHost() {
  const [, setTick] = useState(0);
  const queue = useMemo(() => new CloudAuthQueue<AuthRequest>(() => setTick((n) => n + 1)), []);
  useEffect(() => {
    const handler = (r: AuthRequest) => queue.push(r);
    handlers.push(handler);
    return () => {
      const i = handlers.indexOf(handler);
      if (i >= 0) handlers.splice(i, 1);
      queue.clear();
    };
  }, [queue]);
  const req = queue.current;
  if (!req) return null;
  // Keyed by the request: a new request gets a fresh form (no token or error carries over).
  return <CloudAuthSheet key={req.id} req={req} onFinish={(ok) => queue.finish(req.id, ok)} />;
}

function CloudAuthSheet({ req, onFinish }: { req: AuthRequest; onFinish(ok: boolean): void }) {
  const app = useApp();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // A `needs_auth` retry knows only the provider id: show its label when the host listed it.
  const label = useHostCloud(req.conn.id).providers.find((p) => p.id === req.provider)?.label ?? req.label;

  const run = async (method: 'cloud.auth.set' | 'cloud.auth.import', params: { token: string } | { source: string }) => {
    setBusy(true);
    setError(null);
    try {
      await req.conn.request(method, { provider: req.provider, ...params } as never, { timeoutMs: 60_000 });
      const host = req.conn.id;
      void cloudStores(app).refreshProviders(host);
      onFinish(true);
    } catch (e) {
      setError(needsAuth(e) ? `${t.cloud.authFailed} ${errorMessage(e)}` : errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <Sheet open onClose={() => onFinish(false)} title={t.cloud.signInTo(label)}>
      <CloudAuthForm
        methods={req.methods}
        busy={busy}
        error={error}
        onToken={(token) => void run('cloud.auth.set', { token })}
        onImport={(source) => void run('cloud.auth.import', { source })}
        onHelp={(u) => app.platform.openExternal(u)}
      />
    </Sheet>
  );
}
