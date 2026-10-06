import { memo, useMemo } from 'react';
import { langOf, parseDiff, tokenize } from '../lib/highlight';
import { cx } from './ui';

/**
 * Unified diff with line numbers and light syntax colouring. `gutter="one"` shows a single
 * line-number column (the new line, the old one for deletions) for narrow panels; `bare` drops
 * the frame so the diff runs edge to edge under a sticky header.
 */
export const DiffView = memo(function DiffView({
  diff,
  path,
  fontSize = 12,
  wrap = false,
  gutter = 'both',
  bare = false,
}: {
  diff: string;
  path: string;
  fontSize?: number;
  wrap?: boolean;
  gutter?: 'both' | 'one';
  bare?: boolean;
}) {
  const lines = useMemo(() => parseDiff(diff), [diff]);
  const lang = langOf(path);
  const cols = gutter === 'one' ? 2 : 3;
  return (
    <div className={cx('term overflow-x-auto', !bare && 'rounded-xl border border-border')} style={{ fontSize }}>
      <table className="w-full border-collapse">
        <tbody>
          {lines.map((l, i) => {
            if (l.kind === 'meta') {
              if (bare) return null;
              return (
                <tr key={i}>
                  <td colSpan={cols} className="px-2 text-faint">
                    {l.text}
                  </td>
                </tr>
              );
            }
            if (l.kind === 'hunk') {
              return (
                <tr key={i} className={bare ? 'bg-surface-2/60' : 'bg-accent/10'}>
                  <td colSpan={cols} className={cx('px-2 py-0.5', bare ? 'text-muted' : 'text-accent')}>
                    {l.text}
                  </td>
                </tr>
              );
            }
            const num = 'w-0 select-none px-1.5 text-right align-top tabular-nums';
            return (
              <tr key={i} className={cx(l.kind === 'add' && 'bg-ok/12', l.kind === 'del' && 'bg-danger/12')}>
                {gutter === 'one' ? (
                  <td className={cx(num, 'min-w-[3.5ch] pl-2.5', l.kind === 'add' ? 'text-add/80' : l.kind === 'del' ? 'text-del/80' : 'text-faint')}>{(l.kind === 'del' ? l.oldNo : l.newNo) ?? ''}</td>
                ) : (
                  <>
                    <td className={cx(num, 'text-faint')}>{l.oldNo ?? ''}</td>
                    <td className={cx(num, 'text-faint')}>{l.newNo ?? ''}</td>
                  </>
                )}
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

/** Read-only file text with line numbers and the same colouring (Files viewer). */
export const CodeView = memo(function CodeView({ text, path, fontSize = 12, wrap = false, maxLines = 20_000 }: { text: string; path: string; fontSize?: number; wrap?: boolean; maxLines?: number }) {
  const lang = langOf(path);
  const lines = useMemo(() => {
    const all = text.split('\n');
    if (all.length > 1 && all[all.length - 1] === '') all.pop();
    return all.slice(0, maxLines);
  }, [text, maxLines]);
  return (
    <div className="term overflow-x-auto" style={{ fontSize }}>
      <table className="w-full border-collapse">
        <tbody>
          {lines.map((l, i) => (
            <tr key={i}>
              <td className="w-0 min-w-[3.5ch] select-none pl-2.5 pr-3 text-right align-top tabular-nums text-faint">{i + 1}</td>
              <td className={cx('pr-2 align-top', wrap ? 'whitespace-pre-wrap break-all' : 'whitespace-pre')}>
                {tokenize(l, lang).map((tk, j) => (
                  <span key={j} className={tk.k === 'plain' ? undefined : `tk-${tk.k}`}>
                    {tk.v}
                  </span>
                ))}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
});
