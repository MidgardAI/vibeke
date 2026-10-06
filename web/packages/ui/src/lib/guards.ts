// Input guards for the composer (spec 16 §9.1 Composer): a destructive command needs a second
// tap, and a password prompt on the screen warns that what is typed will not echo.

export interface DestructivePattern {
  reason: string;
  pattern: RegExp;
}

// Word-boundary anchored so look-alikes ("assume", "forced", "dropdown") never trip it. Ordered
// most specific first; the first match's reason is shown. No `g` flags (stateful `.test`).
export const DESTRUCTIVE_PATTERNS: readonly DestructivePattern[] = [
  { reason: 'rm -r (recursive delete)', pattern: /\brm\b[^\n;&|]*\s(?:-[a-z]*r[a-z]*|--recursive)\b/i },
  { reason: 'git push --force', pattern: /\bgit\s+push\b[^\n;&|]*\s(?:--force(?:-with-lease)?|-f)\b/i },
  { reason: 'git reset --hard', pattern: /\bgit\s+reset\b[^\n;&|]*\s--hard\b/i },
  { reason: 'git clean -f', pattern: /\bgit\s+clean\b[^\n;&|]*\s-[a-z]*f/i },
  { reason: 'git checkout/restore discards changes', pattern: /\bgit\s+(?:checkout|restore)\s+(?:--\s+)?\.(?:\s|$)/i },
  { reason: 'git branch -D', pattern: /\bgit\s+branch\b[^\n;&|]*\s-D\b/ },
  { reason: 'DROP TABLE/DATABASE', pattern: /\bdrop\s+(?:table|database|schema)\b/i },
  { reason: 'TRUNCATE TABLE', pattern: /\btruncate\s+table\b/i },
  { reason: 'DELETE without WHERE', pattern: /\bdelete\s+from\s+\w+\s*;?\s*$/i },
  { reason: 'sudo (runs as root)', pattern: /\bsudo\b/i },
  { reason: '--force flag', pattern: /--force\b/i },
  { reason: 'dd (raw disk write)', pattern: /\bdd\b[^\n]*\bof=/i },
  { reason: 'mkfs (format a filesystem)', pattern: /\bmkfs(?:\.\w+)?\b/i },
  { reason: 'chmod/chown -R on a root path', pattern: /\bch(?:mod|own)\s+-R\b[^\n]*\s\/(?:\s|$)/i },
  { reason: 'fork bomb', pattern: /:\(\)\s*\{\s*:\|:&\s*\};:/ },
  { reason: 'redirect to a system path', pattern: /:>\s*\/|>\s*\/(?:\s|$|(?:dev|etc|boot|proc|sys|usr|bin|sbin|lib|var|root)\b)/i },
  { reason: 'kubectl/terraform destroy', pattern: /\b(?:terraform\s+destroy|kubectl\s+delete)\b/i },
];

/** The reason of the first destructive pattern the text matches, or null. */
export function destructiveReason(text: string): string | null {
  for (const { reason, pattern } of DESTRUCTIVE_PATTERNS) if (pattern.test(text)) return reason;
  return null;
}

/** True when the last non-empty screen line is a no-echo prompt (password, passphrase, PIN). */
export function isNoEchoPrompt(screen: string): boolean {
  const lines = screen.split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    const l = lines[i]!.trimEnd();
    if (l.trim() === '') continue;
    return /(?:password|passphrase|passcode|\bpin\b)[^:\n]{0,40}:\s*$/i.test(l) || /^\[sudo\][^\n]*:\s*$/i.test(l);
  }
  return false;
}
