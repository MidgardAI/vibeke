// QR scanning with the BarcodeDetector API where the browser has it (Chrome/Android, Safari 17+
// behind a flag); everywhere else the pair screen falls back to pasting the link.

import { useEffect, useRef, useState } from 'react';
import { t } from '../i18n';
import { Notice } from './ui';

interface Detector {
  detect(src: HTMLVideoElement): Promise<{ rawValue: string }[]>;
}
type DetectorCtor = new (o: { formats: string[] }) => Detector;

export const qrScanSupported = (): boolean =>
  typeof globalThis !== 'undefined' &&
  'BarcodeDetector' in globalThis &&
  typeof navigator !== 'undefined' &&
  !!navigator.mediaDevices?.getUserMedia;

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
    const Ctor = (globalThis as unknown as { BarcodeDetector: DetectorCtor }).BarcodeDetector;
    const detector = new Ctor({ formats: ['qr_code'] });
    const tick = async () => {
      if (stopped || !video.current) return;
      try {
        const codes = await detector.detect(video.current);
        const hit = codes.find((c) => c.rawValue.includes('pair?d=') || /^[A-Za-z0-9_-]{40,}$/.test(c.rawValue));
        if (hit) {
          stopped = true;
          onResult(hit.rawValue);
          return;
        }
      } catch {
        // frame not ready
      }
      setTimeout(() => void tick(), 250);
    };
    navigator.mediaDevices
      .getUserMedia({ video: { facingMode: 'environment' }, audio: false })
      .then(async (s) => {
        stream = s;
        if (stopped) return s.getTracks().forEach((tr) => tr.stop());
        if (video.current) {
          video.current.srcObject = s;
          await video.current.play().catch(() => {});
          void tick();
        }
      })
      .catch((e: Error) => setError(`${t.pair.noCamera} (${e.message})`));
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
