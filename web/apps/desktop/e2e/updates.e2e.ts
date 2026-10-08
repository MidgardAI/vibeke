import { expect, test } from '@playwright/test';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { execFileSync } from 'node:child_process';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import { join } from 'node:path';
import { appRoot, built, hasDisplay, launchApp, type LaunchedApp } from './helpers';

test.skip(!hasDisplay(), 'no display');
test.skip(!built(), 'build the desktop app first');

test('update controls, sidebar states, guarded IPC and encrypted drafts survive app relaunch', async () => {
  let a: LaunchedApp | null = await launchApp();
  let b: LaunchedApp | null = null;
  try {
    await a.page.evaluate(() => { location.hash = '#/settings'; });
    const controls = a.page.getByTestId('desktop-updates');
    await expect(controls.getByText('Check for a new desktop release.')).toBeVisible();
    await controls.getByRole('switch', { name: 'Check for updates automatically' }).click();
    await expect(controls.getByRole('switch', { name: 'Check for updates automatically' })).not.toBeChecked();

    const result = await a.page.evaluate(async () => {
      const bridge = (window as unknown as { vibeke: { invoke(c: string, ...args: unknown[]): Promise<{ ok: boolean; value?: unknown; error?: unknown }> } }).vibeke;
      const refused = await bridge.invoke('vk:updates.download', 'https://evil.example/update').then(() => false, () => true);
      const premature = await bridge.invoke('vk:updates.install');
      const saved = await bridge.invoke('vk:draft.set', 'test-host', 'test-pane', 'Unsent example draft');
      const oversize = await bridge.invoke('vk:draft.set', 'test-host', 'large-pane', 'x'.repeat(262145));
      const blocked = await bridge.invoke('vk:updates.install');
      await bridge.invoke('vk:draft.set', 'test-host', 'large-pane', '');
      return { refused, premature: premature.ok, saved: saved.ok, oversize: oversize.ok, blocked: JSON.stringify(blocked.error).includes('could not be saved') };
    });
    expect(result).toEqual({ refused: true, premature: false, saved: true, oversize: false, blocked: true });
    expect(readFileSync(join(a.userData, 'drafts', 'drafts.bin')).includes(Buffer.from('Unsent example draft'))).toBe(false);

    // UI rendering fixtures, sent through the same main→renderer event as the real controller.
    await a.app.evaluate(({ BrowserWindow }) => {
      for (const w of BrowserWindow.getAllWindows()) w.webContents.send('vk:updates', { status: 'available', currentVersion: '0.2.0', version: '0.3.0', revision: 100, automatic: false });
    });
    await a.page.getByRole('button', { name: 'Update available · v0.3.0' }).click();
    const sheet = a.page.getByRole('dialog', { name: 'Update Vibeke', exact: true });
    await expect(sheet.getByRole('button', { name: 'Download update', exact: true })).toBeVisible();
    await a.app.evaluate(({ BrowserWindow }) => {
      for (const w of BrowserWindow.getAllWindows()) w.webContents.send('vk:updates', { status: 'downloading', currentVersion: '0.2.0', progress: 42, revision: 101 });
    });
    await expect(sheet.getByRole('progressbar', { name: 'Update download' })).toHaveAttribute('value', '42');
    await a.app.evaluate(({ BrowserWindow }) => {
      for (const w of BrowserWindow.getAllWindows()) w.webContents.send('vk:updates', { status: 'ready', currentVersion: '0.2.0', revision: 102 });
    });
    await expect(sheet.getByRole('button', { name: 'Restart and update', exact: true })).toBeVisible();
    await sheet.getByRole('button', { name: 'Later' }).click();
    await expect(sheet).toHaveCount(0);

    await a.app.close();
    b = await launchApp({ VIBEKE_USER_DATA: a.userData });
    const draft = await b.page.evaluate(async () => {
      const bridge = (window as unknown as { vibeke: { invoke(c: string, ...args: unknown[]): Promise<{ value: unknown }> } }).vibeke;
      return (await bridge.invoke('vk:draft.get', 'test-host', 'test-pane')).value;
    });
    expect(draft).toBe('Unsent example draft');
    await b.page.evaluate(() => { location.hash = '#/settings'; });
    await expect(b.page.getByRole('switch', { name: 'Check for updates automatically' })).not.toBeChecked();
  } finally { await b?.close(); await a?.close(); }
});

// Bun and Electron expose different crypto implementations. Exercise the actual Electron
// main process with minisign's default ED signature, without introducing test IPC in the app.
test('Electron verifies prehashed release signatures and rejects tampering', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'vibeke-signature-test-'));
  let a: LaunchedApp | undefined;
  try {
    const module = join(dir, 'verify.cjs');
    execFileSync('bun', ['build', 'src/main/update-release.ts', '--target=node', '--format=cjs', `--outfile=${module}`], { cwd: appRoot });
    const { privateKey, publicKey } = generateKeyPairSync('ed25519');
    const id = Buffer.from('testonly'), data = Buffer.from('release checksum fixture'), comment = 'vibeke v0.3.0';
    const key = Buffer.concat([Buffer.from('Ed'), id, (publicKey.export({ format: 'der', type: 'spki' }) as Buffer).subarray(-32)]).toString('base64');
    const sig = sign(null, createHash('blake2b512').update(data).digest(), privateKey);
    const global = sign(null, Buffer.concat([sig, Buffer.from(comment)]), privateKey);
    const signature = `untrusted comment: test\n${Buffer.concat([Buffer.from('ED'), id, sig]).toString('base64')}\ntrusted comment: ${comment}\n${global.toString('base64')}\n`;
    a = await launchApp();
    const result = await a.app.evaluate(async (_, f) => {
      const { createRequire } = process.getBuiltinModule('module');
      const { verifyMinisign } = createRequire(f.module)(f.module);
      const data = Buffer.from(f.data, 'base64');
      const verified = verifyMinisign(data, f.signature, [f.key]);
      let refused = false;
      try { verifyMinisign(Buffer.concat([data, Buffer.from('tampered')]), f.signature, [f.key]); } catch { refused = true; }
      return { verified, refused, electron: process.versions.electron };
    }, { module, data: data.toString('base64'), signature, key });
    expect(result.verified).toBe(comment);
    expect(result.refused).toBe(true);
    expect(result.electron).toBeTruthy();
  } finally { await a?.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('failed installation clears saved navigation before later activations', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'vibeke-route-test-'));
  let a: LaunchedApp | undefined;
  try {
    const module = join(dir, 'windows.cjs');
    execFileSync('bun', ['build', 'src/main/windows.ts', '--target=node', '--format=cjs', '--external=electron', `--outfile=${module}`], { cwd: appRoot });
    a = await launchApp();
    const result = await a.app.evaluate(async ({ BrowserWindow }, f) => {
      const { createRequire } = process.getBuiltinModule('module');
      const { readFileSync } = process.getBuiltinModule('fs');
      const { Windows } = createRequire(f.module)(f.module);
      const windows = new Windows({ stateFile: f.file });
      windows.main = BrowserWindow.getAllWindows()[0];
      windows.quitting = true;
      windows.prepareUpdate();
      const before = JSON.parse(readFileSync(f.file, 'utf8'));
      windows.cancelUpdate();
      const after = JSON.parse(readFileSync(f.file, 'utf8'));
      return { before, after, quitting: windows.quitting };
    }, { module, file: join(dir, 'state.json') });
    expect(result.before).toHaveProperty('updateRoute');
    expect(result.after).not.toHaveProperty('updateRoute');
    expect(result.quitting).toBe(false);
  } finally { await a?.close(); rmSync(dir, { recursive: true, force: true }); }
});
