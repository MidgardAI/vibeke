// An image from the conversation: a thumbnail that opens full size, fetched on demand through
// `agent.transcript` with the item's `ref` (the transcript list carries no image data). A failed
// or oversized image becomes a small badge. Decoded URLs are kept for a short while so scrolling
// back does not fetch again.

import { createContext, useContext, useEffect, useRef, useState } from 'react';
import { ImageOff } from 'lucide-react';
import { useApp } from '../app/hooks';
import { t } from '../i18n';
import { imageDataUrl } from '../lib/preview';
import { Sheet } from './ui';

/** Where a conversation's images come from: the host and the run. */
export interface ImageSource {
  host: string;
  target: string;
}
export const ImageSourceContext = createContext<ImageSource | null>(null);

/** Largest image the server returns (decoded bytes); bigger ones are not requested. */
export const IMAGE_MAX_BYTES = 4 * 1024 * 1024;
const CACHE_MAX = 24;
const cache = new Map<string, string>();

function remember(key: string, url: string): void {
  cache.delete(key);
  cache.set(key, url);
  if (cache.size > CACHE_MAX) cache.delete(cache.keys().next().value as string);
}

export function TranscriptImage({ mime, imageRef, size }: { mime: string; imageRef: string; size: number | null }) {
  const app = useApp();
  const source = useContext(ImageSourceContext);
  const key = source ? `${source.host}|${source.target}|${imageRef}` : '';
  const tooBig = size !== null && size > IMAGE_MAX_BYTES;
  const [url, setUrl] = useState<string | null>(() => cache.get(key) ?? null);
  const [failed, setFailed] = useState(tooBig);
  const [seen, setSeen] = useState(false);
  const [open, setOpen] = useState(false);
  const boxRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = boxRef.current;
    if (!el || seen) return;
    if (typeof IntersectionObserver === 'undefined') {
      setSeen(true);
      return;
    }
    const io = new IntersectionObserver((es) => es.some((e) => e.isIntersecting) && setSeen(true), { rootMargin: '200px' });
    io.observe(el);
    return () => io.disconnect();
  }, [seen]);

  useEffect(() => {
    if (!source || !seen || url || failed) return;
    let live = true;
    app
      .conn(source.host)
      ?.request('agent.transcript', { target: source.target, image: imageRef })
      .then(
        (r) => {
          if (!live) return;
          const data = imageDataUrl(r.image?.mime ?? mime, r.image?.data_b64);
          if (data) {
            remember(key, data);
            setUrl(data);
          } else setFailed(true);
        },
        () => live && setFailed(true),
      );
    return () => {
      live = false;
    };
  }, [app, source, seen, url, failed, imageRef, mime, key]);

  if (!source) return null;
  if (failed)
    return (
      <div className="inline-flex w-fit items-center gap-1.5 rounded-md bg-surface-2 px-2 py-1 text-xs text-muted" title={tooBig ? t.panel.imageTooLarge : undefined}>
        <ImageOff className="size-3.5" aria-hidden />
        {t.conv.imageFailed}
      </div>
    );
  return (
    <div ref={boxRef} className="flex">
      {url ? (
        <>
          <button
            type="button"
            aria-label={t.conv.imageOpen}
            onClick={() => setOpen(true)}
            className="vk-focus overflow-hidden rounded-lg border border-border bg-surface-2 pointer-coarse:min-h-11"
          >
            <img src={url} alt={t.conv.imageAlt} className="block max-h-48 max-w-[min(100%,320px)] object-contain" onError={() => setFailed(true)} />
          </button>
          <Sheet open={open} onClose={() => setOpen(false)} title={t.conv.imageAlt}>
            <div className="checker flex justify-center rounded-md p-2">
              <img src={url} alt={t.conv.imageAlt} className="max-h-[70vh] max-w-full object-contain" />
            </div>
          </Sheet>
        </>
      ) : (
        <div className="h-24 w-40 animate-pulse rounded-lg bg-surface-2 motion-reduce:animate-none" aria-hidden />
      )}
    </div>
  );
}
