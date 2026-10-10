# Working in this repository

Vibeke is public. Keep committed documentation, examples, screenshots, and release notes suitable for public readers. Use neutral sample names and paths. Do not commit credentials, private operational notes, local deployment metadata, or signing secrets.

Preserve unrelated changes in the shared checkout. Check `git status` and the current branch before editing. Verify the current remote state before pushing. Use the current code and published assets as evidence; design specifications are not proof that a feature shipped.

## Repository map

- `crates/vibeke`: main binary, setup/doctor/update commands, and integration tests in `crates/vibeke/tests/`. Most of them are modules of one test binary, `it` (`tests/it/main.rs`, one module per file). `chaos`, `chaos_gaps` and `timing` stay separate because the nightly and weekly workflows run them in release mode.
- `crates/vk-server`: state actor, JSON-RPC API (`api.rs`), schema registry (`api_schema.rs`), render stream, agents, gateway supervisor. `vk-hold` is the per-pane holder that owns the PTY and survives server restarts. `vk-store` holds SQLite state and scrollback.
- `crates/vk-proto`: wire types. JSON-RPC is `vibeke/1`; the render stream is postcard with `render::PROTOCOL`; `holder.rs` is the holder protocol.
- `crates/vk-term`: VT engine over vendored libghostty-vt, built with Zig by `build.rs` (pin and patches in `vendor/`).
- `crates/vk-tui`, `crates/vk-cli`: TUI client and CLI. `vibeke <noun> <verb> --flag` maps to API namespace, method and params; commands that make several calls live in modules such as `vk-cli/src/verbs.rs`.
- Remote access: `vk-gateway` (host bridge), `vk-relay` (content-blind relay), `vk-e2e` (Noise channel and pairing), `vk-account`, `vk-remote` (SSH bootstrap).
- `crates/vk-config`: config schema (`types.rs`, `default_config.toml`) and default keymap (`keys.rs`, conflict check `check_keys`).
- `web/`: bun workspace. `packages/core` (protocol, no DOM) and `packages/ui` (all app screens) are shared by `apps/pwa` and `apps/desktop`, which only implement `UiPlatform`. `apps/site` is the independent product site. See `web/README.md`.
- `clients/`, `integrations/`: API clients and harness plugins. `spec/` holds design specs cited in code as `spec NN §x`.

## Build and test

- Toolchains are pinned in `mise.toml`. The system `cargo` may be older than the workspace `rust-version`; use `cargo +<pinned version>` or `mise exec -- cargo …`. `vk-term` needs the pinned Zig on `PATH` or in `ZIG`. Its build script caches the built libghostty-vt in `~/.cache/vibeke/libghostty-vt` (`VK_TERM_CACHE_DIR`), so new worktrees and clippy runs skip the Zig build.
- Each worktree has its own `target/`, so its first build is cold. Set `sccache` as `build.rustc-wrapper` in your user `~/.cargo/config.toml` so worktrees and sessions share compiled crates. Avoid several cold builds at once, and run `cargo clean` in worktrees you no longer build. Dev builds keep line tables only; set `CARGO_PROFILE_DEV_DEBUG=2` for a debugger session.
- A new worktree needs `mise trust` before its first build; otherwise cargo falls back to an older toolchain and fails to load the manifest.
- Rust checks: `mise run ci` (fmt, clippy `-D warnings`, cargo-deny, nextest). While iterating, run targeted tests: `cargo nextest run -p <crate> <filter>` or `cargo test -p vibeke --test it <file>::<name>`. For one file of the `it` binary, use `cargo test -p vibeke --test it <file>::` or `cargo nextest run -p vibeke -E 'test(/^<file>::/)'`. A new integration test file needs a `mod <file>;` line in `crates/vibeke/tests/it/main.rs`. In a shared checkout, format with `cargo fmt -p <crate>`, not `--all`. For releases, GitHub CI replaces local full runs (see below).
- Web checks, from `web/`: `bun install --frozen-lockfile`, `bun run typecheck`, `bun run test`. `bun run build` builds the PWA; use `build:site` and `build:desktop` for the others. Commit `web/bun.lock` when dependencies or workspace versions change.
- Generated files fail their tests when stale. Regenerate in this order, then rerun without the variables:
  1. `VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test it api_docs::` (API schema and site reference; `api_clients` compares against it)
  2. `VIBEKE_UPDATE_CLIENTS=1 cargo test -p vibeke --test it api_clients::` (Python and TypeScript clients)
  3. `mise run pi-extension` (`integrations/pi-extension/dist/vibeke.js` is committed and embedded)
- A new API method needs an entry in the method tables of `api_schema.rs` (with its `mutating` flag), a deliberate pane scope (`api::pane_scope_of`), and an integration test or a reasoned entry in `crates/vibeke/tests/api_method_allowlist.txt` (checked by `tests/it/api_method_coverage.rs`).

## Compatibility rules

- `vibeke/1` is additive only. Clients must ignore unknown fields and events. The frozen schema is `docs/api/vibeke-1.frozen.json`; update it with `VIBEKE_UPDATE_API_FREEZE=1`, and a deliberate break also needs `VIBEKE_API_FREEZE_ALLOW_BREAK=1` plus the `api-break-approved` PR label for the schema-diff workflow.
- The render stream is positional (postcard): any field change is breaking and requires bumping `PROTOCOL` in `vk-proto/src/render.rs`.
- A new server must keep talking to holders started by an older version. Do not reorder or remove holder protocol variants or extend frames that older peers decode.

## Code and test conventions

- All state mutations go through `Core::commit` (`vk-server/src/core.rs`): it writes entities and events in one SQLite transaction and only then updates memory.
- Many unit tests sit in sibling `*_tests.rs` files. Most integration tests use `crates/vibeke/tests/it/support/mod.rs::Session`, which runs the real binary in isolated `VIBEKE_*` directories.
- Unix socket paths must stay under 104 bytes (macOS limit). Tests create short directories under `/tmp`; do not root test sockets in a long `TMPDIR`.
- Fix flaky tests at the cause: wait for a condition instead of sleeping, use relative time bounds and free ports, and assume a loaded machine.
- Redact sensitive output with `vk-redact` before it reaches logs, events or debug bundles; event storage does not redact for you.
- Host CLI targets are macOS and Linux only; Windows is desktop-only. Sandboxing is Seatbelt on macOS and bubblewrap on Linux.
- Fuzz targets are `fn(&[u8])`, registered in `vk-fuzz/src/targets.rs`. Do not add fuzzing crates to the main lockfile (`docs/hardening.md`).

## Parallel work and sub-agents

- Several agent sessions may edit the checkout at once. Stage your own paths explicitly and review `git diff --cached`; never `git add -A` or `git commit -a`. If `main` was rewritten, rebase onto it.
- Give each sub-agent its own branch and worktree, rebased on current `main`. Remove worktrees with `git worktree remove` and delete merged branches.
- Sub-agents write code and run `cargo fmt -p <crate>` only. They do not compile, run clippy or run test suites: parallel cold builds slow the machine for everyone. The main agent merges the branches, then builds and tests once at the end.
- Use a faster model for sub-agents that make straightforward changes: Sonnet 5.5 in Claude Code, `gpt-6.1-sol` in Codex. Keep the main model for design work and difficult fixes.
- Before merging to `main`, have a different coding agent review the final diff, read-only:
  - From Claude Code, use Codex: `codex exec -s read-only -m gpt-6-astra "<review prompt>"` (`codex review --base` does not accept a custom prompt).
  - From Codex, use Claude Code: `claude -p --model claude-opus-5-5 --permission-mode plan "<review prompt>"`.
- Fix the findings, review again, then fast-forward `main`.

## Commits

- Commit subjects are plain imperative sentences describing the change, optionally prefixed with an area (`Desktop e2e: …`). No conventional-commit tags and no AI co-author trailers.

## Writing style

Public docs, site copy and release notes follow the Simple English Wikipedia guide (https://simple.wikipedia.org/wiki/Wikipedia:How_to_write_Simple_English_pages):

- Use common words. Explain a technical term the first time it appears, or link to its explanation.
- Use active voice and subject-verb-object sentences.
- Write one idea per sentence. Split sentences joined by "and", "but", "so" or a semicolon. Use at most one subordinate clause.
- Do not use idioms or contractions.
- Clarity is more important than brevity, but do not add filler (see Documentation conventions).
- Docs and UI text may address the reader as "you".

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
- Describe only released UI actions, or ones the maintainer says are about to ship. Device pairing uses the TUI **Devices** view (`prefix+alt+d`, palette **Pair a phone**) or `vibeke gateway pair`; the server manages the gateway after setup. The desktop app offers local connection and gateway-start controls.
- Leave out filler: maturity labels such as "pre-1.0" or "pre-release", statements about what readers do not need (a GitHub account, `sudo`, building the app, starting the gateway by hand), internal build or provenance details (generated-from-source notes, `spec/` references), and sentences that repeat another line or page. Keep real limitations, prerequisites and security warnings.
- Keep host CLI platforms distinct from desktop client platforms. Do not imply that the desktop package bundles the CLI.
- CLI, configuration, and API references are generated. Regenerate them with `VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test it api_docs::` when their source changes.
- Use `docs/desktop-downloads.md` as the shared desktop download table. The installation and desktop guides include it. Update it with each release.

## New release workflow

Read `.github/workflows/release.yml`, `docs/releases.md`, `scripts/release-build.sh`, and `scripts/release-sign.sh` before releasing. They define the current artifact and signing contract. The checklist below supplements those files.

### 1. Prepare a fixed source commit

1. Select the version and update `[workspace.package].version` in `Cargo.toml`, affected workspace entries in `Cargo.lock` (`cargo metadata` refreshes them), and the fallback `VERSION` in `scripts/install.sh`. The Python and TypeScript clients and the pi extension carry their own versions; leave them unchanged.
2. Update `web/apps/desktop/package.json` to the same version. Update the browser app version in `web/apps/pwa/package.json` when publishing that app for the release. `bun install` does not rewrite workspace versions in `web/bun.lock`; edit those entries by hand, then verify `bun install --frozen-lockfile`.
3. Review version references rather than replacing historical versions globally. Old release notes and version-pinned historical download links must keep their original versions.
4. Regenerate API references if needed. Commit the release inputs, record the exact SHA, and push it to `main`. Build release artifacts from this committed state. Do not silently reuse binaries from a different source revision or move an existing published tag.
5. Let GitHub run the checks: the `ci` workflow on that commit runs the `mise run ci` checks in separate lint and test jobs, the release-signing contract test, and the web and desktop checks. It skips the Rust and desktop jobs when a commit changes only web code, release notes or agent notes; its `ci-ok` job gives the overall result. Do not repeat them locally. Tag only after `ci` passes on the release commit. Report failures accurately; do not claim a release is fully green when checks failed or were not run.

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
| Desktop update feed | `latest.yml`, `latest-mac.yml`, `latest-linux.yml`, and a `Vibeke-<version>-*.blockmap` per updatable package; all are covered by the signed `SHA256SUMS` |

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

Keep the public notes in `docs/release-notes/v<version>.md`. Use `v0.1.0.md` as a structural example, not as version data to copy unchanged.

Recommended order:

1. Short introduction and user-visible changes for this version.
2. CLI installation, prerequisites, supported host platforms, and a short first-run example.
3. Direct desktop download table: OS, architecture, and format links.
4. Browser/phone access and the current pairing instructions.
5. Known limitations, upgrade behavior, and links to checksums/signatures.

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
vercel pull --yes --environment production --scope <team>
VERCEL=1 bun run build
vercel deploy --prebuilt --prod --scope <team>
PLAYWRIGHT_BASE_URL=https://vibeke.dev bun run e2e --workers 2
```

GitHub CI runs the site e2e suite on the pushed commit; the production e2e run above checks the deployed site, so skip a local pre-deploy e2e run. A documentation update does not require rebuilding Electron or deploying the browser app. When the browser app changes, follow `web/apps/pwa/README.md`: check it, run `bun run build:vercel`, and deploy its prebuilt output to `vibeke-app`. Build from a committed checkout so the About screen has a traceable build identifier.

### 6. Verify the public result

- Confirm the release tag, asset inventory, signatures, and exact source/build identities. Fetch public download links without a GitHub token.
- Follow `https://vibeke.dev/install.sh`; compare the response with the release's installer and check shell syntax. Verify the downloaded CLI signature/checksum and reported version. Test installation in an isolated test environment without changing the user's installation.
- Check that the home page shows the new version and that its link opens the published release.
- Check every desktop asset link returns successfully and appears on both `/docs/install` and `/docs/desktop`. Confirm the release body matches its committed Markdown source.
- Run website desktop/mobile checks against production. Check for stale `github.com/espen/` links, private-repository instructions, and source-build requirements in normal onboarding.
- For app releases, run `bun scripts/check-app.ts https://app.vibeke.dev` from `web/apps/site/`. From the repository root, run `bun web/packages/core/scripts/check-relay.ts wss://relay.vibeke.dev`. Test pairing with an isolated host and the released CLI when the connection flow changes.
- Report deployed URLs, source SHA, released version, and checks performed. Distinguish HTTP availability from successful installation/pairing, and local test results from pending or failed GitHub CI.

Publishing authorization carries through the requested workflow. Do not ask for repeated confirmation for already-authorized commits, pushes, release edits, or deployments. If signing requires the maintainer's password or an unavailable DNS change, finish independent preparation and ask only for that missing input.
