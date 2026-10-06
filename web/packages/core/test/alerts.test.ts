// Notification text: secrets are redacted from the whole command before it is truncated, for every
// redaction pattern and wherever the secret sits relative to the truncation point.

import { describe, expect, test } from 'bun:test';
import { alertPayload, describeInteraction, type AgentRun, type Dashboard, type Interaction } from '../src';

const run = { id: 'r1', pane: 'p1', harness: 'codex' } as AgentRun;
const dash = { at: 1, session: 's', machine: 'm', workspaces: [], tabs: [], panes: [], runs: [run], interactions: [], tasks: [], notifications_unread: 0 } as unknown as Dashboard;
const approval = (command: string): Interaction =>
  ({ id: 'i1', run: 'r1', pane: 'p1', kind: 'approval', status: 'open', title: 't', action: { tool: 'Bash', command, risk: 'low' } }) as unknown as Interaction;

/** `M` marks the sensitive characters; none may survive into the notification. */
const SECRETS: [string, string][] = [
  ['anthropic', 'sk-ant-api03-' + 'M7k'.repeat(10)],
  ['openai', 'sk-' + 'M7k'.repeat(10)],
  ['openai project', 'sk-proj-' + 'M7k'.repeat(10)],
  ['github token', 'ghp_' + 'M7k'.repeat(12)],
  ['github pat', 'github_pat_' + 'M7k'.repeat(10)],
  ['gitlab', 'glpat-' + 'M7k'.repeat(8)],
  ['aws', 'AKIA' + 'M7KM7KM7KM7KM7KM'],
  ['slack', 'xoxb-' + 'M7k'.repeat(6)],
  ['jwt', 'eyJ' + 'M7k'.repeat(4) + '.eyJ' + 'M7k'.repeat(4) + '.' + 'M7k'.repeat(4)],
  ['authorization', 'Authorization: Basic ' + 'M7k'.repeat(5)],
  ['bearer', 'Bearer ' + 'M7k'.repeat(5)],
  ['url userinfo', 'https://deploy:' + 'M7k'.repeat(4) + '@example.com/repo'],
  ['password double-quoted', 'PASSWORD="hunter2 M7k' + 'M7k'.repeat(10) + '"'],
  ['password single-quoted', "db_password='M7k" + 'M7k'.repeat(10) + "'"],
  ['api key bare', 'API_KEY=' + 'M7k'.repeat(10)],
  ['client secret', 'client_secret: ' + 'M7k'.repeat(10)],
  ['pem', '-----BEGIN PRIVATE KEY-----\n' + 'M7k'.repeat(30) + '\n-----END PRIVATE KEY-----'],
];

const leaks = (s: string) => /M7k|M7K|hunter2/i.test(s);

describe('notification redaction happens before truncation', () => {
  for (const [name, secret] of SECRETS) {
    test(`${name}: no fragment at any offset around the 80-character cut`, () => {
      for (let pad = 0; pad <= 100; pad++) {
        const command = `${'x'.repeat(pad)} ${secret} tail`;
        const title = describeInteraction(dash, approval(command));
        const p = alertPayload([{ key: 'i1', title, url: '#/i/h/i1', urgent: true }], 'full', 'host')!;
        if (leaks(p.title)) throw new Error(`pad ${pad}: ${p.title}`);
        expect(leaks(title)).toBe(false);
      }
    });
  }

  test('a redacted command still reads naturally', () => {
    const t = describeInteraction(dash, approval('PASSWORD="hunter2ZZZ" make deploy'));
    expect(t).toBe('Codex wants to run `PASSWORD="[REDACTED]" make deploy`');
  });
});
