// The macOS menu-bar glyph: the logo's duck rising out of its terminal window, drawn as a
// template image (only alpha counts; macOS tints it for light and dark menu bars). The raster
// logo turns to mush at 18pt, so this is a simplified outline of it. `badge` adds a dot at the
// top right for "something needs you".

import sharp from 'sharp';

const HEAD =
  'M6.1 15 C6.3 12.6 5.2 10.4 5.3 7.6 C5.3 4.6 7 2.6 9.2 2.6 C10.6 2.6 11.5 3.4 12.2 4.4 L16.1 6.5 ' +
  'C16.9 7 16.6 7.9 15.8 7.9 L11.6 8.2 C10.5 8.4 10 9.6 10.1 11 C10.2 12.6 10.6 13.8 10.9 15 Z';

export function trayGlyphSvg(size: number, badge = false): string {
  // The badge and the head are cut out of what lies under them, so each shape stays readable.
  const cut = badge ? '<circle cx="15.2" cy="2.8" r="3.4" fill="black"/>' : '';
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 18 18">
  <defs>
    <mask id="frame"><rect width="18" height="18" fill="white"/>
      <path d="${HEAD}" fill="black" stroke="black" stroke-width="2.2" stroke-linejoin="round"/>${cut}
    </mask>
    <mask id="head"><rect width="18" height="18" fill="white"/>
      <circle cx="9.3" cy="5.2" r="0.8" fill="black"/>${cut}
    </mask>
  </defs>
  <rect mask="url(#frame)" x="1.1" y="7.1" width="15.8" height="9.7" rx="2.4" fill="none" stroke="black" stroke-width="1.35"/>
  <circle mask="url(#frame)" cx="3.3" cy="9.6" r="0.7" fill="black"/>
  <path mask="url(#head)" d="${HEAD}" fill="black"/>
  ${badge ? '<circle cx="15.2" cy="2.8" r="2.3" fill="black"/>' : ''}
</svg>`;
}

export function trayGlyph(size: number, badge = false): Promise<Buffer> {
  return sharp(Buffer.from(trayGlyphSvg(size, badge))).png().toBuffer();
}
