// The Screenshots tab: the workspace's screenshots (agent attachments and browser captures), newest
// first, as a grid of thumbnails. A card opens a full-size viewer with previous / next and Save.
// Images load lazily through `screenshot.get {inline: true}` and are kept in a small byte-bounded
// cache; images over 8 MiB are not fetched. New ones arrive from `screenshot.captured` events
// (app/screenshot-store.ts).

import { useEffect, useMemo, useRef, useState } from 'react';
import { ChevronLeft, ChevronRight, Download, ImageOff, Images } from 'lucide-react';
import type { ScreenshotMeta } from '@vibeke/core';
import { useApp, useHost, useNow } from '../../../app/hooks';
import { screenshotStore, useWorkspaceShots } from '../../../app/screenshot-store';
import { Dialog } from '../../../components/dialog';
import { Button, Empty, IconButton, Spinner, cx } from '../../../components/ui';
import { t } from '../../../i18n';
import { ago, byteSize } from '../../../lib/format';
import { imageDataUrl } from '../../../lib/preview';
import { ByteLru, captionOf, saveName, stepIndex, tooBigToInline } from '../../../lib/screenshots';
import type { WorkspaceRoute } from '../../../router';

const cache = new ByteLru<string>(48, 64 * 1024 * 1024);
const cacheKey = (host: string, s: ScreenshotMeta) => `${host}|${s.id}`;

/** The decoded image of one screenshot, fetched once `enabled` (null until then; `failed` on error). */
function useShotImage(host: string, s: ScreenshotMeta, enabled: boolean): { url: string | null; failed: boolean } {
  const app = useApp();
  const key = cacheKey(host, s);
  const big = tooBigToInline(s);
  const [url, setUrl] = useState<string | null>(() => cache.get(key) ?? null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    setUrl(cache.get(key) ?? null);
    setFailed(false);
  }, [key]);
  useEffect(() => {
    if (!enabled || big || url || failed) return;
    let live = true;
    app
      .conn(host)
      ?.request('screenshot.get', { id: s.id, inline: true })
      .then(
        (r) => {
          if (!live) return;
          const data = imageDataUrl(r.mime || s.mime, r.data_b64);
          if (data) {
            cache.set(key, data, data.length);
            setUrl(data);
          } else setFailed(true);
        },
        () => live && setFailed(true),
      );
    return () => {
      live = false;
    };
  }, [app, host, s.id, s.mime, key, enabled, big, url, failed]);
  return { url, failed };
}

/** Pane id to handle (`p2`) for the host's panes. */
function usePaneHandles(host: string): ReadonlyMap<string, string> {
  const panes = useHost(host)?.dashboard?.panes;
  return useMemo(() => new Map((panes ?? []).map((p) => [p.id, p.handle])), [panes]);
}

export function ScreenshotsTab({ route }: { route: WorkspaceRoute }) {
  const app = useApp();
  const { host, workspace } = route;
  const { list, loaded, error } = useWorkspaceShots(host, workspace);
  const handles = usePaneHandles(host);
  const now = useNow(30_000);
  const [openId, setOpenId] = useState<string | null>(null);

  // While the tab shows, nothing counts as unread (and opening it clears the badge).
  useEffect(() => {
    const s = screenshotStore(app);
    s.setOpen(host, workspace, true);
    return () => s.setOpen(host, workspace, false);
  }, [app, host, workspace]);

  if (!loaded) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <Spinner />
      </div>
    );
  }
  if (!list.length)
    return error ? (
      <Empty
        icon={<ImageOff />}
        title={t.shots.loadFailed}
        action={
          <Button size="sm" onClick={() => void screenshotStore(app).refresh(host, workspace)}>
            {t.shots.retry}
          </Button>
        }
      />
    ) : (
      <Empty icon={<Images />} title={t.shots.emptyTitle} hint={t.shots.emptyHint} />
    );
  return (
    <>
      <div className="vk-scroll min-h-0 flex-1 overflow-y-auto p-2">
        <ul className="grid grid-cols-[repeat(auto-fill,minmax(9.5rem,1fr))] gap-2">
          {list.map((s) => (
            <li key={s.id} className="min-w-0">
              <ShotCard host={host} shot={s} pane={s.pane ? handles.get(s.pane) : undefined} now={now} onOpen={() => setOpenId(s.id)} />
            </li>
          ))}
        </ul>
      </div>
      <Viewer host={host} list={list} openId={openId} handles={handles} now={now} onSelect={setOpenId} onClose={() => setOpenId(null)} />
    </>
  );
}

/** True once the element has come near the viewport (or immediately without IntersectionObserver). */
function useSeen(): [React.RefObject<HTMLDivElement | null>, boolean] {
  const ref = useRef<HTMLDivElement>(null);
  const [seen, setSeen] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el || seen) return;
    if (typeof IntersectionObserver === 'undefined') {
      setSeen(true);
      return;
    }
    const io = new IntersectionObserver((es) => es.some((e) => e.isIntersecting) && setSeen(true), { rootMargin: '200px' });
    io.observe(el);
    return () => io.disconnect();
  }, [seen]);
  return [ref, seen];
}

function ShotCard({ host, shot, pane, now, onOpen }: { host: string; shot: ScreenshotMeta; pane: string | undefined; now: number; onOpen(): void }) {
  const [ref, seen] = useSeen();
  const { url, failed } = useShotImage(host, shot, seen);
  const big = tooBigToInline(shot);
  const name = captionOf(shot);
  return (
    <button
      type="button"
      onClick={onOpen}
      aria-label={t.shots.open(name)}
      className="vk-focus flex w-full flex-col overflow-hidden rounded-lg border border-border bg-surface text-left hover:bg-hover"
    >
      <div ref={ref} className="checker relative aspect-[4/3] w-full overflow-hidden bg-surface-2">
        {url ? (
          <img src={url} alt="" loading="lazy" className="size-full object-cover object-top" />
        ) : big || failed ? (
          <div className="flex size-full flex-col items-center justify-center gap-1 px-2 text-center text-xs text-muted">
            <ImageOff className="size-5" aria-hidden />
            {big ? t.shots.tooLarge : t.shots.failed}
          </div>
        ) : (
          <div className="size-full animate-pulse bg-surface-2 motion-reduce:animate-none" aria-hidden />
        )}
      </div>
      <div className="flex min-w-0 flex-col gap-0.5 px-2 py-1.5">
        <span className="line-clamp-2 break-words text-xs font-medium leading-snug">{name}</span>
        <span className="truncate text-2xs text-muted">{[pane ? t.shots.from(pane) : null, ago(shot.created_at_ms, now)].filter(Boolean).join(' · ')}</span>
      </div>
    </button>
  );
}

function Viewer({
  host,
  list,
  openId,
  handles,
  now,
  onSelect,
  onClose,
}: {
  host: string;
  list: readonly ScreenshotMeta[];
  openId: string | null;
  handles: ReadonlyMap<string, string>;
  now: number;
  onSelect(id: string): void;
  onClose(): void;
}) {
  const index = openId ? list.findIndex((s) => s.id === openId) : -1;
  const shot = index >= 0 ? list[index]! : null;
  // The open screenshot was deleted: close.
  useEffect(() => {
    if (openId && !shot) onClose();
  }, [openId, shot, onClose]);
  const go = (delta: number) => {
    const next = list[stepIndex(index, delta, list.length)];
    if (next) onSelect(next.id);
  };
  useEffect(() => {
    if (!shot) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.altKey || e.ctrlKey || e.metaKey) return;
      if (e.key === 'ArrowLeft') go(-1);
      else if (e.key === 'ArrowRight') go(1);
      else return;
      e.preventDefault();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
  if (!shot) return null;
  return (
    <Dialog
      open
      onClose={onClose}
      label={t.shots.viewer}
      closeOnNavigate
      className="fixed inset-0 z-50 flex items-stretch justify-center sm:items-center sm:p-6"
      panelClassName="relative flex h-full w-full flex-col overflow-hidden bg-surface outline-none sm:h-auto sm:max-h-full sm:max-w-4xl sm:rounded-xl sm:border sm:border-border sm:shadow-[var(--shadow)]"
    >
      <ViewerBody host={host} shot={shot} pane={shot.pane ? handles.get(shot.pane) : undefined} now={now} index={index} count={list.length} go={go} onClose={onClose} />
    </Dialog>
  );
}

function ViewerBody({ host, shot, pane, now, index, count, go, onClose }: { host: string; shot: ScreenshotMeta; pane: string | undefined; now: number; index: number; count: number; go(d: number): void; onClose(): void }) {
  const { url, failed } = useShotImage(host, shot, true);
  const big = tooBigToInline(shot);
  const name = captionOf(shot);
  const meta = [shot.label && shot.label !== name ? shot.label : null, pane ? t.shots.from(pane) : null, ago(shot.created_at_ms, now), shot.width && shot.height ? `${shot.width}×${shot.height}` : null].filter(Boolean).join(' · ');
  return (
    <>
      <div className="flex items-center gap-2 border-b border-border px-3 py-2">
        <div className="min-w-0 flex-1">
          <h2 className="break-words text-sm font-semibold leading-snug">{name}</h2>
          <p className="truncate text-xs text-muted">{meta}</p>
        </div>
        {url && (
          <a
            href={url}
            download={saveName(shot)}
            className="vk-focus inline-flex h-8 shrink-0 items-center gap-1.5 rounded-md border border-border bg-surface-2 px-3 text-sm hover:bg-surface-3 pointer-coarse:h-10 [&>svg]:size-4"
          >
            <Download aria-hidden />
            {t.shots.save}
          </a>
        )}
        <IconButton label={t.close} onClick={onClose}>
          <span aria-hidden>×</span>
        </IconButton>
      </div>
      <div className="checker vk-scroll flex min-h-0 flex-1 items-center justify-center overflow-auto p-2">
        {url ? (
          <img src={url} alt={name} className="max-h-[75vh] max-w-full object-contain" />
        ) : big || failed ? (
          <div className="flex flex-col items-center gap-2 p-8 text-sm text-muted">
            <ImageOff className="size-8" aria-hidden />
            {big ? t.shots.tooLarge : t.shots.failed}
          </div>
        ) : (
          <Spinner />
        )}
      </div>
      <div className="flex items-center justify-between gap-2 border-t border-border px-3 py-2 pb-safe">
        <IconButton label={t.shots.prev} disabled={index <= 0} onClick={() => go(-1)} className={cx('pointer-coarse:size-11')}>
          <ChevronLeft />
        </IconButton>
        <span className="text-xs tabular-nums text-muted">{t.shots.position(index + 1, count)}</span>
        <IconButton label={t.shots.next} disabled={index >= count - 1} onClick={() => go(1)} className={cx('pointer-coarse:size-11')}>
          <ChevronRight />
        </IconButton>
      </div>
    </>
  );
}
