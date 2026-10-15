// QR scanning from the camera. The BarcodeDetector API decodes where the browser has it
// (Chrome/Android); elsewhere (Safari on iPhone and iPad, Firefox) jsQR decodes frames drawn to a
// canvas. jsQR loads on first use, so the app bundle does not carry it.

import { useEffect, useRef, useState } from 'react';
import { t } from '../i18n';
import { Notice } from './ui';

interface Detector {
  detect(src: HTMLVideoElement): Promise<{ rawValue: string }[]>;
}
type DetectorCtor = new (o: { formats: string[] }) => Detector;

/** Decode one video frame: the text of each QR code in it. */
type Decode = (video: HTMLVideoElement) => Promise<string[]>;

/** Camera access is all scanning needs: jsQR covers browsers without BarcodeDetector. */
export const qrScanSupported = (): boolean =>
  typeof navigator !== 'undefined' && !!navigator.mediaDevices?.getUserMedia;

/** Longest side of the frame jsQR reads: enough for a code that fills part of the view. */
const MAX_SIDE = 720;

async function makeDecoder(): Promise<Decode> {
  const g = globalThis as unknown as { BarcodeDetector?: DetectorCtor & { getSupportedFormats?(): Promise<string[]> } };
  if (g.BarcodeDetector) {
    const formats = await g.BarcodeDetector.getSupportedFormats?.().catch(() => [] as string[]);
    if (!formats || formats.includes('qr_code')) {
      const detector = new g.BarcodeDetector({ formats: ['qr_code'] });
      return async (v) => (await detector.detect(v)).map((c) => c.rawValue);
    }
  }
  const { default: jsQR } = await import('jsqr');
  const canvas = document.createElement('canvas');
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  if (!ctx) throw new Error('no canvas');
  return async (v) => {
    const w = v.videoWidth;
    const h = v.videoHeight;
    if (!w || !h) return [];
    const scale = Math.min(1, MAX_SIDE / Math.max(w, h));
    canvas.width = Math.round(w * scale);
    canvas.height = Math.round(h * scale);
    ctx.drawImage(v, 0, 0, canvas.width, canvas.height);
    const img = ctx.getImageData(0, 0, canvas.width, canvas.height);
    const code = jsQR(img.data, img.width, img.height, { inversionAttempts: 'attemptBoth' });
    return code ? [code.data] : [];
  };
}

export function QrScanner({ onResult }: { onResult(text: string): void }) {
  const video = useRef<HTMLVideoElement>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!qrScanSupported()) {
      setError(t.pair.noCamera);
      return;
    }
    let stream: MediaStream | null = null;
    let stopped = false;
    let decode: Decode | null = null;
    const tick = async () => {
      if (stopped || !video.current || !decode) return;
      try {
        const hit = (await decode(video.current)).find((s) => s.includes('pair?d=') || /^[A-Za-z0-9_-]{40,}$/.test(s));
        if (hit && !stopped) {
          stopped = true;
          onResult(hit);
          return;
        }
      } catch {
        // frame not ready
      }
      setTimeout(() => void tick(), 200);
    };
    navigator.mediaDevices
      .getUserMedia({ video: { facingMode: 'environment' }, audio: false })
      .then(async (s) => {
        stream = s;
        if (stopped) return s.getTracks().forEach((tr) => tr.stop());
        decode = await makeDecoder();
        if (stopped || !video.current) return;
        video.current.srcObject = s;
        await video.current.play().catch(() => {});
        void tick();
      })
      .catch((e: Error) => {
        stream?.getTracks().forEach((tr) => tr.stop());
        if (!stopped) setError(`${t.pair.noCamera} (${e.message})`);
      });
    return () => {
      stopped = true;
      stream?.getTracks().forEach((tr) => tr.stop());
    };
  }, []);

  if (error) return <Notice tone="warn">{error}</Notice>;
  return (
    <div className="space-y-2">
      <video ref={video} playsInline muted className="aspect-square w-full rounded-2xl bg-black object-cover" />
      <div className="text-center text-sm text-muted">{t.pair.scanning}</div>
    </div>
  );
}
