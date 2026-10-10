import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { RpcError, needsAuth, type CloudAuthMethod } from '@vibeke/core';
import { CloudAuthForm } from '../src/components/cloud-auth';
import { runWithCloudAuth } from '../src/lib/cloud';

const methods: CloudAuthMethod[] = [
  { kind: 'paste_token', label: 'API token', help_url: 'https://example.com/tokens', hint: 'Starts with sk-' },
  { kind: 'import', source: 'cli', label: 'Import from the provider CLI' },
  { kind: 'env', var: 'SPRITES_TOKEN' },
];

const authError = (provider = 'sprites') =>
  new RpcError('cloud.box.list', { code: -32000, message: 'sign in', data: { kind: 'permission_denied', details: { reason: 'needs_auth', provider, methods } } });

describe('CloudAuthForm', () => {
  const html = renderToStaticMarkup(<CloudAuthForm methods={methods} error="Token rejected" onToken={() => {}} onImport={() => {}} onHelp={() => {}} />);

  test('renders a masked token field and the help button', () => {
    expect(html).toContain('type="password"');
    expect(html).toContain('API token');
    expect(html).toContain('Get a token');
    expect(html).toContain('Starts with sk-');
  });

  test('renders the import button, the env note and the error', () => {
    expect(html).toContain('Import from the provider CLI');
    expect(html).toContain('Using SPRITES_TOKEN from the host');
    expect(html).toContain('Token rejected');
  });

  test('hides the help button for a non-http link', () => {
    const bad = renderToStaticMarkup(<CloudAuthForm methods={[{ kind: 'paste_token', label: 'T', help_url: 'javascript:alert(1)' }]} onToken={() => {}} onImport={() => {}} onHelp={() => {}} />);
    expect(bad).not.toContain('Get a token');
  });

  test('shows the note when the host already uses the variable', () => {
    const env = renderToStaticMarkup(<CloudAuthForm methods={[]} envVar="E2B_API_KEY" onToken={() => {}} onImport={() => {}} onHelp={() => {}} />);
    expect(env).toContain('Using E2B_API_KEY from the host');
  });
});

describe('needsAuth', () => {
  test('reads provider and methods from the error details', () => {
    expect(needsAuth(authError())).toEqual({ provider: 'sprites', methods });
    expect(needsAuth(new RpcError('x', { code: -1, message: 'm', data: { kind: 'conflict' } }))).toBeNull();
    expect(needsAuth(new Error('x'))).toBeNull();
  });
});

describe('runWithCloudAuth', () => {
  test('signs in on needs_auth and retries once', async () => {
    let calls = 0;
    const asked: string[] = [];
    const r = await runWithCloudAuth(
      async () => {
        if (++calls === 1) throw authError();
        return 'ok';
      },
      async (p) => (asked.push(p), true),
    );
    expect(r).toBe('ok');
    expect(calls).toBe(2);
    expect(asked).toEqual(['sprites']);
  });

  test('does not retry when the user closes the sign-in', async () => {
    let calls = 0;
    await expect(
      runWithCloudAuth(
        async () => {
          calls++;
          throw authError();
        },
        async () => false,
      ),
    ).rejects.toBeInstanceOf(RpcError);
    expect(calls).toBe(1);
  });

  test('retries only once', async () => {
    let calls = 0;
    let asks = 0;
    await expect(
      runWithCloudAuth(
        async () => {
          calls++;
          throw authError();
        },
        async () => (asks++, true),
      ),
    ).rejects.toBeInstanceOf(RpcError);
    expect(calls).toBe(2);
    expect(asks).toBe(1);
  });

  test('passes other errors through without signing in', async () => {
    let asked = false;
    await expect(
      runWithCloudAuth(
        async () => {
          throw new Error('boom');
        },
        async () => ((asked = true), true),
      ),
    ).rejects.toThrow('boom');
    expect(asked).toBe(false);
  });
});
