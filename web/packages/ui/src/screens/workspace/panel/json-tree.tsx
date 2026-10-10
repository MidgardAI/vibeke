// A collapsible tree of a JSON document (file viewer preview). The tree is built with a cap on
// the number of values (lib/preview.ts); this draws it with native <details>.

import { useMemo } from 'react';
import { t } from '../../../i18n';
import { buildJsonTree, type JsonNode } from '../../../lib/preview';

const VALUE_CLS: Record<string, string> = { string: 'tk-str', number: 'tk-num', boolean: 'tk-kw', null: 'tk-kw' };

function NodeView({ n, depth }: { n: JsonNode; depth: number }) {
  const key = n.key !== null ? <span className="mr-1 text-info">{n.key}:</span> : null;
  if (n.kind === 'object' || n.kind === 'array') {
    const summary = n.kind === 'array' ? `[${n.size}]` : `{${n.size}}`;
    return (
      <details open={depth < 2}>
        <summary className="cursor-pointer select-none py-px marker:text-faint">
          {key}
          <span className="text-faint">{summary}</span>
        </summary>
        <div className="ml-3 border-l border-border pl-2">
          {n.children.map((c, i) => (
            <NodeView key={i} n={c} depth={depth + 1} />
          ))}
          {n.omitted > 0 && <div className="text-faint">{t.panel.jsonOmitted(n.omitted)}</div>}
        </div>
      </details>
    );
  }
  return (
    <div className="break-all py-px pl-3.5">
      {key}
      <span className={VALUE_CLS[n.kind]}>{n.kind === 'string' ? JSON.stringify(n.text) : n.text}</span>
    </div>
  );
}

export function JsonTreeView({ value, fontSize }: { value: unknown; fontSize: number }) {
  const tree = useMemo(() => buildJsonTree(value), [value]);
  return (
    <div className="term px-2 py-1.5" style={{ fontSize }}>
      {tree.capped && <div className="mb-1 text-xs text-faint">{t.panel.jsonShown(tree.shown, tree.total)}</div>}
      <NodeView n={tree.root} depth={0} />
    </div>
  );
}
