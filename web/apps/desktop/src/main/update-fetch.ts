import { net } from 'electron';
import type { FetchBytes } from './update-release';

function checkedUrl(url: string): URL {
  const u = new URL(url);
  if (u.protocol !== 'https:' || u.username || u.password) throw new Error('Update downloads require HTTPS');
  return u;
}

// Chromium networking honors system/PAC proxies. net.fetch cannot expose manual redirects
// (it rejects with "Redirect was cancelled"), so inspect ClientRequest's redirect event.
export const electronFetchBytes: FetchBytes = (url, limit = 2 * 1024 * 1024) => new Promise((resolve, reject) => {
  const target = checkedUrl(url);
  const request = net.request({ url: target.toString(), redirect: 'manual', useSessionCookies: false });
  let settled = false, redirects = 0;
  const timer = setTimeout(() => fail(new Error('Release check timed out')), 30_000);
  const fail = (error: Error) => {
    if (settled) return;
    settled = true; clearTimeout(timer); request.abort(); reject(error);
  };
  request.setHeader('User-Agent', 'Vibeke-Updater');
  request.setHeader('Accept', target.hostname === 'api.github.com' ? 'application/vnd.github+json' : 'application/octet-stream');
  request.on('redirect', (_status, _method, location) => {
    try {
      checkedUrl(location);
      if (++redirects > 5) throw new Error('Too many release redirects');
      request.followRedirect();
    } catch (e) { fail(e as Error); }
  });
  request.on('error', fail);
  request.on('response', (response) => {
    if (response.statusCode < 200 || response.statusCode >= 300) { fail(new Error(`Release check failed (HTTP ${response.statusCode})`)); return; }
    if (Number(response.headers['content-length'] ?? 0) > limit) { fail(new Error('Release metadata is too large')); return; }
    const chunks: Buffer[] = [];
    let total = 0;
    response.on('data', (chunk: Buffer) => {
      if (settled) return;
      total += chunk.length;
      if (total > limit) { fail(new Error('Release metadata is too large')); return; }
      chunks.push(chunk);
    });
    response.on('error', fail);
    response.on('aborted', () => fail(new Error('Release response was interrupted')));
    response.on('end', () => {
      if (settled) return;
      settled = true; clearTimeout(timer); resolve(Buffer.concat(chunks));
    });
  });
  request.end();
});
