// "Copy output": the visible terminal text as plain text (no ANSI), without trailing blanks.

import { stripAnsi } from './ansi';

export function plainOutput(text: string): string {
  return stripAnsi(text)
    .replace(/\r\n?/g, '\n')
    .split('\n')
    .map((l) => l.replace(/[ \t]+$/, ''))
    .join('\n')
    .replace(/\n+$/, '');
}
