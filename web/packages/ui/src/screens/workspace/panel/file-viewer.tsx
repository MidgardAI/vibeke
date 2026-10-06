// Read-only file viewer for the Files tab (`fs.read`): highlighted text with line numbers, and
// placeholders for secrets, binaries, empty and oversized files. Lazy-loaded with the tab.

import { useEffect, useState } from 'react';
import { ArrowLeft, RefreshCw } from 'lucide-react';
import type { FsRead } from '@vibeke/core';
import { useApp, usePrefs } from '../../../app/hooks';
import { CodeView } from '../../../components/diff';
import { FileIcon } from '../../../components/file-icon';
import { IconButton, Notice, Spinner } from '../../../components/ui';
import { t } from '../../../i18n';
import { errorMessage } from '../../../lib/answer';
import { splitPath } from '../../../lib/changes';
import { byteSize } from '../../../lib/format';

export default function FileViewer({ host, pane, path, onBack }: { host: string; pane: string; path: string; onBack(): void }) {
  const app = useApp();
  const prefs = usePrefs();
  const [file, setFile] = useState<FsRead | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);
  useEffect(() => {
    let live = true;
    setFile(null);
    setError(null);
    app
      .conn(host)
      ?.request('fs.read', { pane, path })
      .then(
        (f) => live && setFile(f),
        (e) => live && setError(errorMessage(e)),
      );
    return () => {
      live = false;
    };
  }, [app, host, pane, path, nonce]);
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
        {file?.size != null && <span className="shrink-0 text-xs tabular-nums text-faint">{byteSize(file.size)}</span>}
        <IconButton label={t.panel.refresh} onClick={() => setNonce((n) => n + 1)} className="size-7">
          <RefreshCw className="size-3.5" />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
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
        {file && !file.secret && file.binary && <Notice className="m-3">{t.panel.binaryFile}</Notice>}
        {file && !file.secret && !file.binary && file.text == null && <Notice className="m-3">{t.panel.tooLarge(byteSize(file.size ?? 0))}</Notice>}
        {file && !file.secret && !file.binary && file.text != null && (
          <>
            {file.truncated && (
              <Notice tone="warn" className="m-2">
                {t.panel.truncatedFile}
              </Notice>
            )}
            {file.text === '' ? <div className="p-4 text-sm text-muted">{t.panel.emptyFile}</div> : <CodeView text={file.text} path={path} fontSize={prefs.termFont} wrap={prefs.wrap} />}
          </>
        )}
      </div>
    </div>
  );
}
