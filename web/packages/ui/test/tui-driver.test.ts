import { expect, test } from 'bun:test';
import { FakeClock, flush } from '../../core/test/helpers';
import { TuiDriver } from '../src/lib/tui-driver';
test('protocol deadlines run without paint callbacks and stop when idle', async () => {
  const clock = new FakeClock(); let ticks = 0, sends = 0;
  const driver = new TuiDriver({ clock, tick: () => ++ticks === 1 ? 20 : undefined, flush: () => { sends++; }, paint() {}, effects() {}, failed(e) { throw e; } });
  driver.wake(); driver.wake(); await flush();
  expect(ticks).toBe(1); expect(sends).toBe(1);
  await clock.advance(20); expect(ticks).toBe(2); expect(sends).toBe(2);
  expect(clock.pending).toBe(0);
  driver.dispose(); driver.wake(); await flush(); expect(ticks).toBe(2);
});
test('dispose cancels queued work and timers', async () => {
  const clock = new FakeClock(); let ticks = 0;
  const driver = new TuiDriver({ clock, tick: () => { ticks++; return 10; }, flush() {}, paint() {}, effects() {}, failed(e) { throw e; } });
  driver.wake(); await flush(); driver.dispose(); await clock.advance(100); expect(ticks).toBe(1); expect(clock.pending).toBe(0);
});
