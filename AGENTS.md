# Working in this repository

Vibeke is public. Keep committed documentation, examples, screenshots, and release notes suitable for public readers. Use neutral sample names and paths. Do not commit credentials, private operational notes, local deployment metadata, or signing secrets.

Preserve unrelated changes in the shared checkout. Check `git status` and the current branch before editing. Verify the current remote state before pushing. Use the current code and published assets as evidence; design specifications are not proof that a feature shipped.

## Public entry points

- Repository and releases: `https://github.com/MidgardAI/vibeke`.
- Product site and docs: `https://vibeke.dev` (Vercel project `vibeke-dev`).
- CLI installer: `https://vibeke.dev/install.sh`.
- Browser app: `https://app.vibeke.dev` (separate Vercel project `vibeke-app`).
- Relay: `https://relay.vibeke.dev` (separate service, not the Vercel app).

Discover the deployment team from the local Vercel project link or authenticated account. Keep personal account names and private infrastructure details out of public instructions. Do not move the relay or combine the two Vercel projects as part of a documentation deployment.

## Documentation conventions

- Edit canonical Markdown in `docs/site/src/`; the site reads it directly. Add new chapters to both `docs/site/src/SUMMARY.md` and `web/apps/site/content/manifest.ts`.
- Installation and quickstart pages lead with published releases and the terminal interface. Source builds belong in the development guide.
- A fresh `vibeke` session creates its first workspace in the current directory. Users can run their normal `claude`, `codex`, or other agent command in a pane. Do not require a second terminal for ordinary onboarding.
- Describe only released UI actions. Device pairing currently uses `vibeke gateway pair`; the server manages the gateway after setup. The desktop app offers local connection and gateway-start controls.
- The TUI Devices view (`prefix+alt+d`, palette **Pair a phone**: pair with a QR code, list and revoke devices) and the fixed 📱 connected-devices count in the status bar are on `main` but not in v0.1.0. In the first release that includes them, describe them in `docs/site/src/mobile.md` (pairing and **Manage devices**), then delete this note.
- Keep host CLI platforms distinct from desktop client platforms. Do not imply that the desktop package bundles the CLI.
- CLI, configuration, and API references are generated. Regenerate them with `VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test api_docs` when their source changes.
- Use `docs/desktop-downloads.md` as the shared desktop download table. The installation and desktop guides include it. Update it with each release.

## New release workflow

Read `.github/workflows/release.yml`, `docs/releases.md`, `scripts/release-build.sh`, and `scripts/release-sign.sh` before releasing. They define the current artifact and signing contract. The checklist below supplements those files.

### 1. Prepare a fixed source commit

1. Select the version and update `[workspace.package].version` in `Cargo.toml`, affected workspace entries in `Cargo.lock`, and the fallback `VERSION` in `scripts/install.sh`.
2. Update `web/apps/desktop/package.json` to the same version. Update the browser app version in `web/apps/pwa/package.json` when publishing that app for the release. Refresh lockfiles as necessary and verify frozen-lockfile installation.
3. Review version references rather than replacing historical versions globally. Old release notes and version-pinned historical download links must keep their original versions.
4. Run the applicable checks, including `mise run ci`, `mise run repro-check`, and release-signing checks described in `docs/releases.md`. Regenerate API references if needed. Report failures accurately; do not claim a release is fully green when checks failed or were not run.
5. Commit the release inputs and record the exact SHA. Build release artifacts from this committed state. Do not silently reuse binaries from a different source revision or move an existing published tag.

For a packaging rehearsal, use `gh workflow run release.yml --ref main`. Optional `-f component=cli` or `-f component=desktop` limits the build. Manual runs retain workflow artifacts and do not create releases.

### 2. Build a draft release

From the intended release commit, create and push `v<version>`. The tag workflow builds the host CLI and Electron packages, then creates an **unsigned draft**.

Wait for all required packaging jobs and inspect the actual draft asset list. Currently expected:

| Product | Platforms and asset names |
| --- | --- |
| Host CLI | `vibeke-macos-aarch64`, `vibeke-linux-x86_64`, `vibeke-linux-aarch64`, plus their `.sha256` files |
| macOS desktop | `Vibeke-<version>-mac-arm64.dmg`, `.zip`, and `Vibeke-<version>-mac-x64.dmg`, `.zip` |
| Linux desktop | `Vibeke-<version>-linux-x86_64.AppImage`, `Vibeke-<version>-linux-amd64.deb` |
| Windows desktop | `Vibeke-<version>-win-x64.exe` |
| Installer | `install.sh`, pinned by the workflow to the tag's version |

Do not advertise unsupported architectures or assets that were not produced. Desktop packaging runs `verify:package` for Electron fuses, ASAR layout, and applicable macOS code signatures.

Download all draft assets into a fresh `dist/<version>/` directory:

```sh
gh release download v<version> --repo MidgardAI/vibeke --dir dist/<version>
```

Local alternative: `mise exec -- sh scripts/release-build.sh <version>`. This builds CLI artifacts only and skips unavailable toolchains. It recreates `dist/<version>/`; do not run it over downloaded or signed release files. Assemble and verify all desktop and CLI artifacts before signing.

### 3. Sign locally with minisign

Install `minisign` with the platform package manager. Use the existing signing script:

```sh
scripts/release-sign.sh dist/<version>
```

The script uses the current key at `~/.vibeke-release-keys/vibeke-2026.key`. Minisign prompts for its password on the terminal. Let the maintainer enter it directly; never read the secret file, request the password in chat, or place it in arguments, logs, environment variables, or CI.

The script produces and verifies four files:

- `SHA256SUMS`: checksums for all CLI and desktop downloads.
- `SHA256SUMS.minisig`: signed checksum list, with trusted comment `vibeke v<version>`.
- `manifest.json`: version and CLI artifact URLs/checksums for remote bootstrap.
- `manifest.json.minisig`: signature whose trusted comment also includes `version:<version>`.

It verifies signatures against the public key embedded in `crates/vk-remote/src/bootstrap.rs`. Public keys must agree with `scripts/install.sh` and `keys/*.pub`. Use `scripts/tests/release-sign-test.sh` to check the signing contract with throwaway test keys; never use its key overrides for a real release.

Use `--key next` only as part of the documented key-rotation sequence. Do not replace release signing with `VIBEKE_ALLOW_UNSIGNED=1`. Minisign signatures and OS publisher signing/notarization are different; report their status separately.

Upload the four verified outputs to the draft:

```sh
gh release upload v<version> --repo MidgardAI/vibeke \
  dist/<version>/SHA256SUMS dist/<version>/SHA256SUMS.minisig \
  dist/<version>/manifest.json dist/<version>/manifest.json.minisig
```

Download a fresh verification copy and verify both signatures and artifact checksums before publication. If an artifact changes, regenerate and verify both signed metadata sets together. Never replace a public binary while retaining old checksums or signatures.

### 4. Write release notes and update downloads

Keep the public notes in `docs/release-notes/v<version>.md`. Use `v0.1.0.md` as a structural example, not as version or provenance data to copy unchanged.

Recommended order:

1. Short introduction and user-visible changes for this version.
2. CLI installation, prerequisites, supported host platforms, and a short first-run example.
3. Direct desktop download table: OS, architecture, and format links.
4. Browser/phone access and the current pairing instructions.
5. Known limitations, upgrade behavior, and links to checksums/signatures.
6. An expandable `<details>` section for exact build provenance and release-validation evidence. Preserve material caveats; do not hide a mismatch between source tags and shipped binaries behind a cleanup of wording.

Use public installation instructions without GitHub login. Pin the CLI version in version-specific notes:

```sh
curl -fsSL https://vibeke.dev/install.sh | VIBEKE_VERSION=<version> sh
```

The variable belongs on `sh`, which runs the installer, not on `curl`.

Update `docs/desktop-downloads.md` with the release version and exact asset URLs obtained from `gh release view --json assets`. Use explicit labels such as Apple silicon (ARM64), Intel (x86_64), AppImage, DEB, DMG, ZIP, and EXE. Keep its links pinned to the named release. Do not put a versioned filename beneath `releases/latest/download/`: it breaks when the latest tag changes but the filename does not.

Set the release body with a file to preserve Markdown and shell examples:

```sh
gh release edit v<version> --repo MidgardAI/vibeke \
  --notes-file docs/release-notes/v<version>.md
```

Replace the workflow's unsigned-draft placeholder before publishing. State the real signing and automatic-update limitations. Do not reuse historical CI results or provenance as evidence for a new release.

### 5. Publish and deploy

After verification, publish the draft with `gh release edit v<version> --repo MidgardAI/vibeke --draft=false`. Confirm whether it is the intended latest stable release; prereleases must not silently become the default installer target.

Update the version shown on the home page: set `version` and `url` in `web/apps/site/src/lib/release.ts` to the published stable release. The site test (`bun run test`) fails when it does not match the newest `docs/release-notes/v<version>.md`. Do not change it for a prerelease or before the release is published.

The website's `/install.sh` route is a 302 redirect, configured in `web/apps/site/vite.config.ts`, to `https://github.com/MidgardAI/vibeke/releases/latest/download/install.sh`. It uses `Cache-Control: no-store`. Keep this endpoint stable; the published release supplies the version-pinned installer. Do not maintain a stale second copy of the script in website public assets.

Commit the documentation and download-table changes, push the authorized branch, and deploy the website from that committed state. From `web/apps/site/`:

```sh
bun run typecheck
bun run test
bun run build
bun run e2e --workers 2
vercel pull --yes --environment production --scope <team>
VERCEL=1 bun run build
vercel deploy --prebuilt --prod --scope <team>
PLAYWRIGHT_BASE_URL=https://vibeke.dev bun run e2e --workers 2
```

A documentation update does not require rebuilding Electron or deploying the browser app. When the browser app changes, follow `web/apps/pwa/README.md`: check it, run `bun run build:vercel`, and deploy its prebuilt output to `vibeke-app`. Build from a committed checkout so the About screen has a traceable build identifier.

### 6. Verify the public result

- Confirm the release tag, asset inventory, signatures, and exact source/build identities. Fetch public download links without a GitHub token.
- Follow `https://vibeke.dev/install.sh`; compare the response with the release's installer and check shell syntax. Verify the downloaded CLI signature/checksum and reported version. Test installation in an isolated test environment without changing the user's installation.
- Check that the home page shows the new version and that its link opens the published release.
- Check every desktop asset link returns successfully and appears on both `/docs/install` and `/docs/desktop`. Confirm the release body matches its committed Markdown source.
- Run website desktop/mobile checks against production. Check for stale `github.com/espen/` links, private-repository instructions, and source-build requirements in normal onboarding.
- For app releases, run `bun scripts/check-app.ts https://app.vibeke.dev` from `web/apps/site/`. From the repository root, run `bun web/packages/core/scripts/check-relay.ts wss://relay.vibeke.dev`. Test pairing with an isolated host and the released CLI when the connection flow changes.
- Report deployed URLs, source SHA, released version, and checks performed. Distinguish HTTP availability from successful installation/pairing, and local test results from pending or failed GitHub CI.

Publishing authorization carries through the requested workflow. Do not ask for repeated confirmation for already-authorized commits, pushes, release edits, or deployments. If signing requires the maintainer's password or an unavailable DNS change, finish independent preparation and ask only for that missing input.
