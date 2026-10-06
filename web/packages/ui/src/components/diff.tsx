import { memo, useMemo } from 'react';
import { langOf, parseDiff, tokenize } from '../lib/highlight';
import { cx } from './ui';

/** Unified diff with line numbers and light syntax colouring. */
export const DiffView = memo(function DiffView({ diff, path, fontSize = 12, wrap = false }: { diff: string; path: string; fontSize?: number; wrap?: boolean }) {
  const lines = useMemo(() => parseDiff(diff), [diff]);
  const lang = langOf(path);
  return (
    <div className="term overflow-x-auto rounded-xl border border-border" style={{ fontSize }}>
      <table className="w-full border-collapse">
        <tbody>
          {lines.map((l, i) => {
            if (l.kind === 'meta') {
              return (
                <tr key={i}>
                  <td colSpan={3} className="px-2 text-faint">
                    {l.text}
                  </td>
                </tr>
              );
            }
            if (l.kind === 'hunk') {
              return (
                <tr key={i} className="bg-accent/10">
                  <td colSpan={3} className="px-2 py-0.5 text-accent">
                    {l.text}
                  </td>
                </tr>
              );
            }
            return (
              <tr key={i} className={cx(l.kind === 'add' && 'bg-ok/12', l.kind === 'del' && 'bg-danger/12')}>
                <td className="w-0 select-none px-1.5 text-right align-top text-faint">{l.oldNo ?? ''}</td>
                <td className="w-0 select-none px-1.5 text-right align-top text-faint">{l.newNo ?? ''}</td>
                <td className={cx('pr-2 align-top', wrap ? 'whitespace-pre-wrap break-all' : 'whitespace-pre')}>
                  <span className={cx('select-none', l.kind === 'add' ? 'text-ok' : l.kind === 'del' ? 'text-danger' : 'text-faint')}>
                    {l.kind === 'add' ? '+' : l.kind === 'del' ? '-' : ' '}
                  </span>
                  {tokenize(l.text, lang).map((tk, j) => (
                    <span key={j} className={tk.k === 'plain' ? undefined : `tk-${tk.k}`}>
                      {tk.v}
                    </span>
                  ))}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
});
