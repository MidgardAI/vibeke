// Reproducible transfer-size measurements for the generated browser terminal.
import { readFileSync } from 'node:fs';
import { gzipSync, brotliCompressSync, constants } from 'node:zlib';
const root = new URL('../apps/pwa/public/', import.meta.url);
const manifest = JSON.parse(readFileSync(new URL('tui/manifest.json', root), 'utf8')) as { moduleUrl: string };
const file = new URL(manifest.moduleUrl.replace(/^\//, '').replace(/\.js$/, '_bg.wasm'), root);
const bytes = readFileSync(file);
console.log(JSON.stringify({ wasm: manifest.moduleUrl.replace(/\.js$/, '_bg.wasm'), rawBytes: bytes.length,
  gzipBytes: gzipSync(bytes, { level: 9 }).length,
  brotliBytes: brotliCompressSync(bytes, { params: { [constants.BROTLI_PARAM_QUALITY]: 11 } }).length,
}, null, 2));
