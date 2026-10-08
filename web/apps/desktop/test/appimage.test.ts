import { describe, expect, test } from 'bun:test';
import { isVibekeAppImage } from '../src/main/appimage';

describe('isVibekeAppImage', () => {
  const env = { APPIMAGE: '/home/u/Apps/Vibeke-0.3.0-linux-x86_64.AppImage', APPDIR: '/tmp/.mount_VibekeAb12' };
  test('accepts the running Vibeke AppImage', () => {
    expect(isVibekeAppImage(env, '/tmp/.mount_VibekeAb12/vibeke', 'linux')).toBe(true);
  });
  test('rejects variables inherited from another AppImage', () => {
    const other = { APPIMAGE: '/home/u/Apps/Terminal.AppImage', APPDIR: '/tmp/.mount_TermXy' };
    expect(isVibekeAppImage(other, '/usr/lib/vibeke/vibeke', 'linux')).toBe(false);
    expect(isVibekeAppImage(env, '/usr/lib/vibeke/vibeke', 'linux')).toBe(false);
  });
  test('needs both variables, Linux, and a mount prefix match', () => {
    expect(isVibekeAppImage({ APPIMAGE: env.APPIMAGE }, '/tmp/.mount_VibekeAb12/vibeke', 'linux')).toBe(false);
    expect(isVibekeAppImage(env, '/tmp/.mount_VibekeAb12/vibeke', 'darwin')).toBe(false);
    expect(isVibekeAppImage(env, '/tmp/.mount_VibekeAb12-evil/vibeke', 'linux')).toBe(false);
  });
});
