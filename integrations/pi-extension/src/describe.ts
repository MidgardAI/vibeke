// Redaction and small helpers (DESIGN §3). Pure; no host or network access.
import { isAbsolute, resolve } from "node:path";

export const MAX_INPUT_BYTES = 8 * 1024;
const SECRET_KEY = /(key|token|secret|password|authorization)/i;
const MAX_DEPTH = 8;

function redactValue(v: unknown, depth: number, seen: WeakSet<object>): unknown {
  if (v === null || typeof v !== "object") {
    return typeof v === "bigint" || typeof v === "function" || typeof v === "symbol" ? String(v) : v;
  }
  if (seen.has(v as object)) return "[circular]";
  if (depth >= MAX_DEPTH) return "[depth]";
  seen.add(v as object);
  let out: unknown;
  if (Array.isArray(v)) {
    out = v.map((x) => redactValue(x, depth + 1, seen));
  } else {
    const o: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
      o[k] = SECRET_KEY.test(k) ? "[redacted]" : redactValue(val, depth + 1, seen);
    }
    out = o;
  }
  seen.delete(v as object);
  return out;
}

/** Redact secret-looking keys; truncate inputs whose JSON exceeds 8 KiB. */
export function redactInput(input: unknown): unknown {
  try {
    const r = redactValue(input, 0, new WeakSet());
    const json = JSON.stringify(r);
    if (json !== undefined && json.length > MAX_INPUT_BYTES) {
      return { _truncated: true, _bytes: json.length, preview: json.slice(0, MAX_INPUT_BYTES) };
    }
    return r;
  } catch {
    return { _unserializable: true };
  }
}

export const MAX_PROMPT_BYTES = 8 * 1024;

/**
 * The user's request text, bounded at `maxBytes` of UTF-8 (cut on a character boundary).
 * `truncated` is true only when something was cut, so tracking never mistakes a cut prompt for
 * the verbatim request.
 */
export function boundedPrompt(s: unknown, maxBytes = MAX_PROMPT_BYTES): { prompt?: string; truncated: boolean } {
  if (typeof s !== "string" || s.length === 0) return { truncated: false };
  const bytes = new TextEncoder().encode(s);
  if (bytes.length <= maxBytes) return { prompt: s, truncated: false };
  let end = maxBytes;
  while (end > 0 && (bytes[end] & 0xc0) === 0x80) end--; // don't split a UTF-8 sequence
  return { prompt: new TextDecoder().decode(bytes.subarray(0, end)), truncated: true };
}

export function preview(s: unknown, n: number): string | undefined {
  return typeof s === "string" && s.length > 0 ? s.slice(0, n) : undefined;
}

const FILE_TOOL = /(^|[_-])(write|edit|multiedit|patch)/i;

/** Absolute path touched by a write/edit style tool call, if any. */
export function fileChangePath(tool: string, input: unknown, cwd?: string): string | undefined {
  if (!FILE_TOOL.test(tool) || !input || typeof input !== "object") return undefined;
  const o = input as Record<string, unknown>;
  const p = [o.path, o.file_path, o.filePath, o.file].find((x) => typeof x === "string" && x.length > 0) as
    | string
    | undefined;
  if (!p) return undefined;
  return isAbsolute(p) ? p : resolve(cwd ?? process.cwd(), p);
}

export const RATE_LIMIT_RE = /overloaded|rate.?limit|429|5\d\d|timeout/i;
