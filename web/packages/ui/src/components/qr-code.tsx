import { useMemo } from 'react';
import { encode } from 'uqr';
import { cx } from './ui';

/** A QR code drawn as one SVG path (no innerHTML). Dark modules on a white quiet zone. */
export function QrCode({ value, className, label }: { value: string; className?: string; label?: string }) {
  const qr = useMemo(() => {
    try {
      return encode(value, { ecc: 'L', border: 2 });
    } catch {
      return null;
    }
  }, [value]);
  if (!qr) return null;
  const n = qr.size;
  let d = '';
  qr.data.forEach((row, y) => {
    let x = 0;
    while (x < n) {
      if (!row[x]) {
        x++;
        continue;
      }
      let w = 1;
      while (x + w < n && row[x + w]) w++;
      d += `M${x} ${y}h${w}v1h-${w}z`;
      x += w;
    }
  });
  return (
    <svg
      role="img"
      aria-label={label ?? 'QR code'}
      viewBox={`0 0 ${n} ${n}`}
      shapeRendering="crispEdges"
      className={cx('block rounded-xl bg-white', className)}
    >
      <path d={d} fill="#000" />
    </svg>
  );
}
