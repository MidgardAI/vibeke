// Secret redaction for text that leaves the app's view (notifications), mirroring
// crates/vk-redact/src/lib.rs `redact`. Best-effort pattern matching, not a guarantee.

export const REDACTED = '[REDACTED]';

type Rule = { kind: 'whole' | 'prefix' | 'assign'; re: RegExp };

const RULES: Rule[] = [
  // Private key PEM blocks, including truncated ones (no END line).
  { kind: 'whole', re: /-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|$)/g },
  // Anthropic before OpenAI: both start with `sk-`.
  { kind: 'whole', re: /sk-ant-[A-Za-z0-9_-]{8,}/g },
  { kind: 'whole', re: /sk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}/g },
  { kind: 'whole', re: /\bgh[pousr]_[A-Za-z0-9]{30,}/g },
  { kind: 'whole', re: /\bgithub_pat_[A-Za-z0-9_]{20,}/g },
  { kind: 'whole', re: /\bglpat-[A-Za-z0-9_-]{16,}/g },
  { kind: 'whole', re: /\b(?:AKIA|ASIA|AGPA|AIDA|AROA|ANPA)[A-Z0-9]{16}\b/g },
  { kind: 'whole', re: /\bxox[abprs]-[A-Za-z0-9-]{8,}/g },
  // JWT: header and payload are both base64url JSON objects, so both start with `eyJ`.
  { kind: 'whole', re: /\beyJ[A-Za-z0-9_-]{6,}\.eyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]*/g },
  // Authorization header or field (any scheme).
  { kind: 'prefix', re: /(\bauthorization["']?\s*[:=]\s*["']?)(?:(?:bearer|basic|token)\s+)?[^\s"',;]{4,}/gi },
  // Loose `Bearer <token>`.
  { kind: 'prefix', re: /(\bbearer\s+)[A-Za-z0-9._~+/=-]{8,}/gi },
  // URLs with userinfo: scheme://user:pass@host
  { kind: 'prefix', re: /(\b[a-z][a-z0-9+.-]*:\/\/)[^\s/:@]+:[^\s/@]+@/gi },
  // password=…, token: "…", api_key=…, client_secret=…
  {
    kind: 'assign',
    re: /\b([a-z0-9_.-]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|credentials?))(["']?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s"',;&]+)/gi,
  },
];

/** Redact known secret patterns (same rules as the Rust `vk_redact::redact`). */
export function redact(input: string): string {
  let cur = input;
  for (const { kind, re } of RULES) {
    if (kind === 'whole') cur = cur.replace(re, REDACTED);
    else if (kind === 'prefix') cur = cur.replace(re, (whole: string, p: string) => `${p}${REDACTED}${whole.endsWith('@') ? '@' : ''}`);
    else
      cur = cur.replace(re, (whole: string, key: string, delim: string, value: string) => {
        if (value.startsWith(REDACTED) || value.startsWith(`"${REDACTED}`)) return whole;
        const q = value[0] === '"' || value[0] === "'" ? value[0] : '';
        return `${key}${delim}${q}${REDACTED}${q}`;
      });
  }
  return cur;
}
