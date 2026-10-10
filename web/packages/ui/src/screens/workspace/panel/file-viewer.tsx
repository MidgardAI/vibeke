// Read-only file viewer for the Files tab (`fs.read`): highlighted text with line numbers, png,
// jpeg, gif and webp images, and placeholders for secrets, binaries, empty and oversized files. Markdown, SVG and JSON files
// also have a Preview (rendered Markdown whose relative links open other files, the SVG as an
// image from a blob URL, JSON as a collapsible tree). A link such as `src/a.ts:42` opens at its
// line. Lazy-loaded with the tab.

import { useEffect, useMemo, useRef, useState } from 'react';
import { ArrowLeft, RefreshCw } from 'lucide-react';
import type { FsRead } from '@vibeke/core';
import { useApp, usePrefs } from '../../../app/hooks';
import { CodeView } from '../../../components/diff';
import { FileIcon } from '../../../components/file-icon';
import { LinkContext, type LinkOps } from '../../../components/link-context';
import { Markdown } from '../../../components/markdown';
import { IconButton, Notice, Segmented, Spinner } from '../../../components/ui';
import { t } from '../../../i18n';
import { errorMessage } from '../../../lib/answer';
import { splitPath } from '../../../lib/changes';
import { takeFileLine } from '../../../lib/file-focus';
import { byteSize } from '../../../lib/format';
import { safeHref } from '../../../lib/markdown';
import { canPreview, imageDataUrl, isImagePath, parseJson, previewKind, resolveRelativeLink, type PreviewKind } from '../../../lib/preview';
import { JsonTreeView } from './json-tree';

type Mode = 'source' | 'preview';

export default function FileViewer({ host, pane, path, onBack, onOpen }: { host: string; pane: string; path: string; onBack(): void; onOpen?: (path: string) => void }) {
  const app = useApp();
  const prefs = usePrefs();
  const [file, setFile] = useState<FsRead | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);
  const [mode, setMode] = useState<Mode | null>(null);
  const [focusLine, setFocusLine] = useState<number | null>(null);
  const bodyRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    let live = true;
    setFile(null);
    setError(null);
    app
      .conn(host)
      ?.request('fs.read', isImagePath(path) ? { pane, path, as: 'image' } : { pane, path })
      .then(
        (f) => live && setFile(f),
        (e) => live && setError(errorMessage(e)),
      );
    return () => {
      live = false;
    };
  }, [app, host, pane, path, nonce]);

  // A new file starts in its natural mode; a link with a line number shows the source.
  useEffect(() => {
    setMode(null);
    setFocusLine(null);
  }, [path]);
  useEffect(() => {
    if (!file) return;
    const line = takeFileLine(path);
    if (line) {
      setFocusLine(line);
      setMode('source');
    }
  }, [file, path]);
  useEffect(() => {
    if (!focusLine || !file) return;
    bodyRef.current?.querySelector(`[data-line="${focusLine}"]`)?.scrollIntoView({ block: 'center' });
  }, [focusLine, file, mode]);

  const kind: PreviewKind | null = previewKind(path);
  const text = file && !file.secret && !file.binary ? (file.text ?? null) : null;
  const previewable = !!kind && text !== null && text !== '' && !!file && canPreview(kind, file.truncated);
  const shown: Mode = previewable ? (mode ?? (kind === 'markdown' ? 'preview' : 'source')) : 'source';

  const imageUrl = file && !file.secret && file.binary ? imageDataUrl(file.mime, file.data_b64) : null;

  const { dir, name } = splitPath(path);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="sticky top-0 z-10 flex h-9 shrink-0 items-center gap-1.5 border-b border-border bg-bg pl-1 pr-1">
        <IconButton label={t.back} onClick={onBack} className="size-7">
          <ArrowLeft className="size-4" />
        </IconButton>
        <FileIcon path={path} />
        <div className="flex min-w-0 flex-1 items-baseline gap-1.5" title={path}>
          <span className="shrink-0 truncate text-sm font-medium">{name}</span>
          {dir && <span className="min-w-0 truncate text-xs text-muted">{dir.replace(/\/$/, '')}</span>}
        </div>
        {previewable && (
          <Segmented
            label={t.panel.viewMode}
            value={shown}
            onChange={(v: Mode) => setMode(v)}
            options={[
              { value: 'source', label: t.panel.viewSource },
              { value: 'preview', label: t.panel.viewPreview },
            ]}
          />
        )}
        {file?.size != null && <span className="shrink-0 text-xs tabular-nums text-faint">{byteSize(file.size)}</span>}
        <IconButton label={t.panel.refresh} onClick={() => setNonce((n) => n + 1)} className="size-7">
          <RefreshCw className="size-3.5" />
        </IconButton>
      </div>
      <div ref={bodyRef} className="min-h-0 flex-1 overflow-auto">
        {error && (
          <Notice tone="danger" className="m-3">
            {error}
          </Notice>
        )}
        {!file && !error && (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        )}
        {file?.secret && <Notice className="m-3">{t.panel.secretFile}</Notice>}
        {file && !file.secret && file.binary && imageUrl && <ImagePreview url={imageUrl} name={name} />}
        {file && !file.secret && file.binary && !imageUrl && (
          <Notice className="m-3">{file.mime && file.truncated ? t.panel.imageTooLarge : t.panel.binaryFile}</Notice>
        )}
        {file && !file.secret && !file.binary && file.text == null && <Notice className="m-3">{t.panel.tooLarge(byteSize(file.size ?? 0))}</Notice>}
        {file && !file.secret && !file.binary && file.text != null && (
          <>
            {file.truncated && (
              <Notice tone="warn" className="m-2">
                {t.panel.truncatedFile}
              </Notice>
            )}
            {file.text === '' ? (
              <div className="p-4 text-sm text-muted">{t.panel.emptyFile}</div>
            ) : shown === 'preview' && kind ? (
              <Preview kind={kind} text={file.text} path={path} onOpen={onOpen} fontSize={prefs.termFont} />
            ) : (
              <CodeView text={file.text} path={path} fontSize={prefs.termFont} wrap={prefs.wrap} focusLine={focusLine} />
            )}
          </>
        )}
      </div>
    </div>
  );
}

function Preview({ kind, text, path, onOpen, fontSize }: { kind: PreviewKind; text: string; path: string; onOpen?: (path: string) => void; fontSize: number }) {
  if (kind === 'markdown') return <MarkdownPreview text={text} path={path} onOpen={onOpen} />;
  if (kind === 'svg') return <SvgPreview text={text} />;
  return <JsonPreview text={text} fontSize={fontSize} />;
}

function MarkdownPreview({ text, path, onOpen }: { text: string; path: string; onOpen?: (path: string) => void }) {
  const app = useApp();
  // Relative links open the named file in this viewer; plain paths in the text stay plain.
  const ops = useMemo<LinkOps>(
    () => ({
      fileFor: () => null,
      hrefFile: onOpen ? (href) => resolveRelativeLink(path, href) : () => null,
      openFile: (p) => onOpen?.(p),
      openUrl: (url) => {
        const safe = safeHref(url);
        if (safe) app.platform.openExternal(safe);
      },
    }),
    [app, path, onOpen],
  );
  return (
    <LinkContext.Provider value={ops}>
      <div className="mx-auto w-full max-w-[780px] px-4 py-3">
        <Markdown text={text} className="text-[14px] leading-[1.65] text-fg" />
      </div>
    </LinkContext.Provider>
  );
}

/** The SVG as an image from a blob URL: it never becomes part of the page, so its scripts cannot run. */
function SvgPreview({ text }: { text: string }) {
  const [state, setState] = useState<{ url: string } | 'failed' | null>(null);
  useEffect(() => {
    let url: string | null = null;
    try {
      url = URL.createObjectURL(new Blob([text], { type: 'image/svg+xml' }));
      setState({ url });
    } catch {
      setState('failed');
    }
    return () => {
      if (url) URL.revokeObjectURL(url);
    };
  }, [text]);
  const [broken, setBroken] = useState(false);
  useEffect(() => setBroken(false), [text]);
  if (!state) return null;
  if (state === 'failed' || broken) return <Notice className="m-3">{t.panel.previewFailed}</Notice>;
  return (
    <div className="checker flex min-h-full items-center justify-center p-4">
      <img src={state.url} alt={t.panel.svgAlt} className="max-h-full max-w-full" onError={() => setBroken(true)} />
    </div>
  );
}

/** A raster image on a checkerboard, shrunk to fit; a broken image shows the preview notice. */
function ImagePreview({ url, name }: { url: string; name: string }) {
  const [broken, setBroken] = useState(false);
  useEffect(() => setBroken(false), [url]);
  if (broken) return <Notice className="m-3">{t.panel.previewFailed}</Notice>;
  return (
    <div className="checker flex min-h-full items-center justify-center p-4">
      <img src={url} alt={t.panel.imageAlt(name)} className="max-h-full max-w-full object-contain" onError={() => setBroken(true)} />
    </div>
  );
}

function JsonPreview({ text, fontSize }: { text: string; fontSize: number }) {
  const parsed = useMemo(() => parseJson(text), [text]);
  if (!parsed.ok) return <Notice className="m-3">{t.panel.previewFailed}</Notice>;
  return <JsonTreeView value={parsed.value} fontSize={fontSize} />;
}
