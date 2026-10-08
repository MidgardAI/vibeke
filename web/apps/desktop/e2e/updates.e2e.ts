import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { built, hasDisplay, launchApp, type LaunchedApp } from './helpers';

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
    expect(readFileSync(join(a.userData, 'drafts', 'vault.bin')).includes(Buffer.from('Unsent example draft'))).toBe(false);

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
