// Platform label sent at pairing ("iOS", "Android", …) and a default device name.

export interface Detected {
  name: string;
  device: string;
  ios: boolean;
}

export function detectPlatform(ua: string, platform: string, touchPoints: number): Detected {
  const ipad = /iPad/.test(ua) || (platform === 'MacIntel' && touchPoints > 1);
  if (/iPhone|iPod/.test(ua)) return { name: 'iOS', device: 'iPhone', ios: true };
  if (ipad) return { name: 'iPadOS', device: 'iPad', ios: true };
  if (/Android/.test(ua)) return { name: 'Android', device: /Mobile/.test(ua) ? 'Android phone' : 'Android tablet', ios: false };
  if (/Mac/.test(platform) || /Mac OS X/.test(ua)) return { name: 'macOS', device: 'Mac', ios: false };
  if (/Win/.test(platform)) return { name: 'Windows', device: 'Windows PC', ios: false };
  if (/Linux/.test(platform) || /Linux/.test(ua)) return { name: 'Linux', device: 'Linux', ios: false };
  return { name: 'Web', device: 'Browser', ios: false };
}

/**
 * Apple WebKit: Safari on macOS and every browser on iOS/iPadOS (they all use WebKit). It revokes
 * Web Push permission for pushes that show no notification.
 */
export function isAppleWebKit(ua: string, platform: string, touchPoints: number): boolean {
  if (detectPlatform(ua, platform, touchPoints).ios) return true;
  return /Macintosh|Mac OS X/.test(ua) && /Safari\//.test(ua) && !/Chrome\/|Chromium\/|CriOS\/|Edg\/|OPR\/|Firefox\/|FxiOS\//.test(ua);
}
