import { memo, useMemo, type ReactNode } from 'react';
import { parseMarkdown, type Block, type Inline } from '../lib/markdown';
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
      return (
        <pre>
          <code>{b.v}</code>
        </pre>
      );
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
