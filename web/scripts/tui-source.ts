// Fingerprint the workspace packages in the browser target's Cargo dependency graph.
// The builder records these paths so packaging a prebuilt module does not require Rust.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync, existsSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const root = resolve(fileURLToPath(new URL('../../', import.meta.url)));

interface Metadata {
  packages: { id: string; name: string; manifest_path: string; source: string | null }[];
  resolve: { nodes: { id: string; deps: { pkg: string; dep_kinds: { kind: string | null }[] }[] }[] };
}

export function sourceInputs(metadata: Metadata, directory = root): string[] {
  const tui = metadata.packages.find((p) => p.name === 'vk-tui');
  if (!tui) throw new Error('Cargo metadata has no vk-tui package');
  const nodes = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
  const seen = new Set<string>();
  const visit = (id: string) => {
    if (seen.has(id)) return;
    seen.add(id);
    for (const dep of nodes.get(id)?.deps ?? []) {
      if (dep.dep_kinds.some((kind) => kind.kind !== 'dev')) visit(dep.pkg);
    }
  };
  visit(tui.id);
  return [
    'Cargo.toml', 'Cargo.lock', 'mise.toml', '.cargo',
    'scripts/build-wasm-tui.sh', 'web/scripts/tui-source.ts', 'web/scripts/tui-module-api.ts',
    // vk-agents embeds these files from outside its package directory.
    'integrations/pi-extension/dist/vibeke.js', 'integrations/opencode-plugin/vibeke.ts',
    ...metadata.packages.filter((p) => seen.has(p.id) && p.source === null)
      .map((p) => relative(directory, dirname(p.manifest_path))),
  ].sort();
}

export function tuiSourceDigest(inputs: string[], directory = root): string {
  const files = execFileSync('git', ['ls-files', '-z', '--cached', '--others', '--exclude-standard', '--', ...inputs], { cwd: directory }).toString().split('\0').filter(Boolean).sort();
  const hash = createHash('sha256');
  for (const file of [...new Set(files)]) {
    const path = resolve(directory, file);
    const bytes = existsSync(path) ? readFileSync(path) : Buffer.from('deleted');
    hash.update(`${file}\0${bytes.length}\0`).update(bytes);
  }
  return hash.digest('hex');
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv[2] === 'inputs') {
    const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--locked', '--format-version=1', '--filter-platform=wasm32-unknown-unknown'], { cwd: root, maxBuffer: 32 * 1024 * 1024 }).toString()) as Metadata;
    console.log(JSON.stringify(sourceInputs(metadata)));
  } else {
    const inputs = process.argv[2];
    if (!inputs) throw new Error('Pass inputs or the recorded source input paths as JSON');
    console.log(tuiSourceDigest(JSON.parse(inputs)));
  }
}
