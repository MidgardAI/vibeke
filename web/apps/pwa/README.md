# Vibeke browser app

Production: https://app.vibeke.dev. Vercel project: `your-team/vibeke-app`.
This project serves the static PWA. The public relay runs separately at `https://relay.vibeke.dev`.
The marketing site and documentation use the `your-team/vibeke-dev` project at `https://vibeke.dev`.

## Build and deploy

Install dependencies from `web/` with `bun install --frozen-lockfile`.
From `web/apps/pwa/`:

```sh
bun run typecheck
bun run test
vercel link --yes --project vibeke-app --scope your-team
bun run build:vercel
vercel deploy --prebuilt --prod --scope your-team
```

The build records the Git commit in the app's About screen. Deploy from a committed checkout.
`build:vercel` writes the Vercel Build Output API files without uploading source or requiring remote workspace dependencies.
The service worker, HTML, and manifest revalidate. Hashed assets use immutable caching.
Hash-based routes keep pairing invitations in the browser fragment.

Configure the `app` DNS record using the target shown by `vercel domains inspect app.vibeke.dev --scope your-team`.
The domain must also be assigned to `vibeke-app` with Vercel deployment protection disabled for the public production app.

## Pair a host

With a Vibeke session running:

```sh
vibeke gateway pair --relay https://relay.vibeke.dev --app-url https://app.vibeke.dev
```

Open the generated link and confirm the device fingerprint. See the [public guide](https://vibeke.dev/docs/mobile).
Before publishing, verify the app shell, pairing route, manifest, service worker activation, and an encrypted relay connection.

Run `bun scripts/check-app.ts https://app.vibeke.dev` from `web/apps/site/` to check desktop/mobile rendering and the offline shell.
Run `bun web/packages/core/scripts/check-relay.ts wss://relay.vibeke.dev` from the repository root for an encrypted transport check.
