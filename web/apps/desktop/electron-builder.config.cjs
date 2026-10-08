// Packaging (spec 16 §16.2): macOS dmg+zip (arm64, x64), Linux AppImage+deb, Windows nsis.
// Signing and notarization come from the environment only, never from this file:
//   macOS signing    CSC_LINK (+ CSC_KEY_PASSWORD), or a Developer ID identity in the keychain
//   notarization     APPLE_API_KEY + APPLE_API_KEY_ID + APPLE_API_ISSUER, or
//                    APPLE_ID + APPLE_APP_SPECIFIC_PASSWORD + APPLE_TEAM_ID, or APPLE_KEYCHAIN_PROFILE
//   Windows signing  WIN_CSC_LINK (+ WIN_CSC_KEY_PASSWORD)
//   update feed      public GitHub release assets; signed metadata is verified in main
// `bun run dist:dir` builds an app directory signed ad hoc (no Developer ID; runs locally only).

const env = process.env;
const notarize = !!(env.APPLE_API_KEY || env.APPLE_ID || env.APPLE_KEYCHAIN_PROFILE);
// A real identity only when one is configured (CSC_LINK / CSC_NAME, or VIBEKE_MAC_SIGN=1 to pick
// the keychain's Developer ID). Otherwise sign ad hoc: Apple silicon kills unsigned binaries,
// and applying Electron fuses invalidates Electron's own linker signature.
const realIdentity = !!(env.CSC_LINK || env.CSC_NAME || env.VIBEKE_MAC_SIGN);
// The update feed is baked into the bundle (resources/app-update.yml); the app trusts nothing else.
const feed = 'https://github.com/MidgardAI/vibeke/releases/latest/download';

/** @type {import('electron-builder').Configuration} */
module.exports = {
  appId: 'dev.vibeke.desktop',
  productName: 'Vibeke',
  artifactName: 'Vibeke-${version}-${os}-${arch}.${ext}',
  copyright: 'Copyright © Vibeke',
  directories: { output: 'dist', buildResources: 'build' },
  // Main, preload and renderer are fully bundled into out/: no node_modules ship.
  files: ['out/**/*', '!out/**/*.map', 'package.json'],
  asar: true,
  npmRebuild: false,
  protocols: [{ name: 'Vibeke', schemes: ['vibeke'] }],
  electronFuses: {
    runAsNode: false,
    enableCookieEncryption: true,
    enableNodeOptionsEnvironmentVariable: false,
    enableNodeCliInspectArguments: false,
    enableEmbeddedAsarIntegrityValidation: true,
    onlyLoadAppFromAsar: true,
    grantFileProtocolExtraPrivileges: false,
  },
  publish: [{ provider: 'generic', url: feed }],
  mac: {
    category: 'public.app-category.developer-tools',
    icon: 'build/icon.png',
    target: [
      { target: 'dmg', arch: ['arm64', 'x64'] },
      { target: 'zip', arch: ['arm64', 'x64'] },
    ],
    identity: realIdentity ? undefined : '-',
    // Hardened runtime (required for notarization) only with a real identity: ad-hoc frameworks
    // would fail library validation.
    hardenedRuntime: realIdentity,
    gatekeeperAssess: false,
    entitlements: 'build/entitlements.mac.plist',
    entitlementsInherit: 'build/entitlements.mac.plist',
    notarize,
    extendInfo: {
      NSMicrophoneUsageDescription: 'Vibeke records voice input for your agents when you hold the microphone button.',
    },
  },
  dmg: { sign: false },
  linux: {
    target: ['AppImage', 'deb'],
    executableName: 'vibeke-desktop',
    category: 'Development',
    icon: 'build/icons',
    maintainer: 'Vibeke',
    synopsis: 'Your agents and terminals, end-to-end encrypted.',
    mimeTypes: ['x-scheme-handler/vibeke'],
  },
  deb: { packageName: 'vibeke-desktop', depends: ['libsecret-1-0'] },
  win: { target: ['nsis'], icon: 'build/icon.png' },
  nsis: { oneClick: false, perMachine: false, allowToChangeInstallationDirectory: true },
};
