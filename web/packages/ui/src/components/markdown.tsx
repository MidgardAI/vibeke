import { memo, useMemo, type ReactNode } from 'react';
import { parseMarkdown, type Block, type Inline } from '../lib/markdown';
import { tokenize } from '../lib/highlight';
import { useApp } from '../app/hooks';
import { ErrorBoundary } from './error-boundary';

function InlineView({ nodes }: { nodes: Inline[] }) {
  const app = useApp();
  return (
    <>
      {nodes.map((n, i): ReactNode => {
        switch (n.t) {
          case 'text':
            return n.v;
          case 'code':
            return <code key={i}>{n.v}</code>;
          case 'strong':
            return (
              <strong key={i}>
                <InlineView nodes={n.c} />
              </strong>
            );
          case 'em':
            return (
              <em key={i}>
                <InlineView nodes={n.c} />
              </em>
            );
          case 'del':
            return (
              <del key={i}>
                <InlineView nodes={n.c} />
              </del>
            );
          case 'link':
            return (
              <a
                key={i}
                href={n.href}
                rel="noopener noreferrer"
                target="_blank"
                onClick={(e) => {
                  e.preventDefault();
                  app.platform.openExternal(n.href);
                }}
              >
                <InlineView nodes={n.c} />
              </a>
            );
        }
      })}
    </>
  );
}

function BlockView({ b }: { b: Block }): ReactNode {
  switch (b.t) {
    case 'h': {
      const Tag = (`h${Math.min(4, b.level)}`) as 'h1' | 'h2' | 'h3' | 'h4';
      return (
        <Tag>
          <InlineView nodes={b.c} />
        </Tag>
      );
    }
    case 'p':
      return (
        <p>
          <InlineView nodes={b.c} />
        </p>
      );
    case 'code':
      return <CodeBlock lang={b.lang} code={b.v} />;
    case 'quote':
      return (
        <blockquote>
          <Blocks blocks={b.c} />
        </blockquote>
      );
    case 'hr':
      return <hr />;
    case 'raw':
      return <p className="whitespace-pre-wrap">{b.v}</p>;
    case 'list': {
      const items = b.items.map((it, i) => (
        <li key={i}>
          {it.checked !== null && <input type="checkbox" checked={it.checked} readOnly className="mr-1.5 align-middle" />}
          <Blocks blocks={it.c} tight />
        </li>
      ));
      return b.ordered ? <ol start={b.start}>{items}</ol> : <ul>{items}</ul>;
    }
  }
}

const LANG_ALIAS: Record<string, string> = {
  typescript: 'ts',
  javascript: 'js',
  tsx: 'ts',
  jsx: 'js',
  python: 'py',
  rust: 'rs',
  shell: 'sh',
  bash: 'sh',
  zsh: 'sh',
  console: 'sh',
  golang: 'go',
  ruby: 'rb',
  yml: 'yaml',
  markdown: 'md',
  text: 'txt',
  plaintext: 'txt',
};
/** Highlighting is per line and cheap, but bounded anyway (agent output is untrusted). */
const HIGHLIGHT_MAX = 400;

/** A fenced code block, highlighted with the diff tokenizer when its language is known. */
function CodeBlock({ lang, code }: { lang: string; code: string }) {
  const l = lang.trim().toLowerCase().split(/\s+/)[0] ?? '';
  const key = LANG_ALIAS[l] ?? l;
  const lines = useMemo(() => {
    if (!key || key === 'txt' || key === 'md') return null;
    const ls = code.split('\n');
    return ls.length > HIGHLIGHT_MAX ? null : ls.map((line) => tokenize(line, key));
  }, [code, key]);
  return (
    <pre data-lang={key || undefined}>
      <code>
        {lines
          ? lines.map((toks, i) => (
              <span key={i}>
                {toks.map((tk, j) => (
                  <span key={j} className={tk.k === 'plain' ? undefined : `tk-${tk.k}`}>
                    {tk.v}
                  </span>
                ))}
                {i < lines.length - 1 ? '\n' : null}
              </span>
            ))
          : code}
      </code>
    </pre>
  );
}

function Blocks({ blocks, tight }: { blocks: Block[]; tight?: boolean }) {
  if (tight && blocks.length === 1 && blocks[0]!.t === 'p') return <InlineView nodes={blocks[0]!.c} />;
  return (
    <>
      {blocks.map((b, i) => (
        <BlockView key={i} b={b} />
      ))}
    </>
  );
}

function MarkdownBody({ text }: { text: string }) {
  const blocks = useMemo(() => parseMarkdown(text), [text]);
  return <Blocks blocks={blocks} />;
}

/**
 * Sanitized Markdown: no raw HTML, http(s) links only, rendered as React elements. The input is
 * untrusted (agent output): parsing is depth/work bounded, and anything that still throws while
 * parsing or rendering falls back to the literal text instead of taking the screen down.
 */
export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  return (
    <div className={`md break-words ${className ?? ''}`}>
      <ErrorBoundary resetKey={text} fallback={<p className="whitespace-pre-wrap">{text}</p>}>
        <MarkdownBody text={text} />
      </ErrorBoundary>
    </div>
  );
});
