# Releases

## What `mise run dist` produces

`mise run dist` (`scripts/dist.sh`) builds release binaries for the three supported targets and
writes them to `dist/<version>/` (git-ignored). `<version>` is the workspace version in
`Cargo.toml`.

| Artifact | Target triple | Built with |
|---|---|---|
| `vibeke-macos-aarch64` | `aarch64-apple-darwin` | `cargo build --release` |
| `vibeke-linux-x86_64` | `x86_64-unknown-linux-musl` | `cargo zigbuild --release` |
| `vibeke-linux-aarch64` | `aarch64-unknown-linux-musl` | `cargo zigbuild --release` |

Each binary gets a `<name>.sha256` sidecar (`sha256sum` format), and `SHA256SUMS` lists all three.
The same files are copied to `~/.cache/vibeke/releases/<version>/`, which is where `vibeke ssh`
looks for the artifact it pushes to a remote and where `vibeke update` looks for a newer local
build. Set `VIBEKE_RELEASES_DIR` to use another cache root.

The Linux builds are static musl binaries, so they run on any distribution without installing
anything. Zig 0.16 (pinned in `mise.toml`) is the C toolchain for the vendored libghostty-vt and
for linking.

## Integrity today

No release key exists yet, so nothing is cryptographically signed and this build embeds no trusted
keys (`vk_remote::bootstrap::TRUSTED_KEYS` is empty; `verify_signature()` returns
`NoTrustedKeys`). What is verified:

1. **The expected checksum comes from a file next to the artifact, never from the artifact.** For
   `vibeke-<target>` that is its entry in `SHA256SUMS` in the same directory, else the
   `vibeke-<target>.sha256` sidecar. No checksum file means the artifact is refused, even with the
   opt-in below. The artifact's actual sha256 must equal it.
2. **Signature or explicit opt-in.** An artifact is accepted only if `SHA256SUMS.minisig` verifies
   against a public key embedded in the binary (not possible until a release key exists), or the
   user sets `VIBEKE_ALLOW_UNSIGNED=1`. With the opt-in, a loud warning naming the artifact and its
   sha256 is printed. A sidecar checksum is never covered by a signature, so sidecar-only
   artifacts always need the opt-in.

Consumers:

- `vibeke ssh` applies both checks to the local artifact (`~/.cache/vibeke/releases/<v>/` or
  `$VIBEKE_ARTIFACT_DIR`); one that fails is ignored. Pushing the running binary itself (when the
  remote matches this platform) also requires `VIBEKE_ALLOW_UNSIGNED=1`; since no external checksum
  exists for it, its hash only protects the transfer. The remote re-checks the sha256 of the
  uploaded file before switching `current`, and keeps the previous version on mismatch. The switch
  is `ln -s versions/<v> current.new && mv -Tf current.new current` (atomic rename) where `mv -T`
  works (GNU coreutils, modern busybox); otherwise it falls back to `ln -sfn`, which is not atomic.
- `vibeke update` (cached artifact or `--from`) applies both checks before anything else, and
  never executes the candidate (`--version`) until they pass. A cached artifact's version is its
  directory name; for `--from` it is read from the verified binary.
- `scripts/install.sh` verifies the downloaded binary against `SHA256SUMS` before installing.

These checks catch corruption, truncation and a swapped binary next to an untouched checksum file.
They do not prove who built the file: anyone who can replace both the binary and `SHA256SUMS` in
the directory can pass the opt-in path, which is why it is opt-in and loud.

## Signing (release CI, not implemented yet)

The release workflow signs `SHA256SUMS` with [minisign](https://jedisct1.github.io/minisign/) and
publishes `SHA256SUMS`, `SHA256SUMS.minisig` and the binaries as GitHub release assets. Signing the
checksum file (rather than each binary) keeps one signature covering every artifact.

Key handling:

- The minisign key pair is generated once, offline, by the maintainer. The secret key never lives
  in the repository.
- The secret key (password-protected) and its password are stored as GitHub Actions secrets that
  are only available to the release workflow on protected tags (`v*`), in a protected environment
  that requires maintainer approval.
- The public key is committed to the repository (for example `docs/vibeke.pub`) and embedded in
  `scripts/install.sh` and in the `vibeke` binary, so `vibeke update` can verify a signature
  without trusting the download host.
- Rotation: publish the new public key in a release signed by the old key, then switch CI to the
  new secret. A lost or leaked key means a new key published out of band (README and release
  notes), with a clear version cut-over.

Verifier behavior once signing exists: the installer and `vibeke update` verify
`SHA256SUMS.minisig` with the embedded public key, then verify the binary against `SHA256SUMS`.
A missing signature is an error for downloaded releases; locally built artifacts without a
signature need `VIBEKE_ALLOW_UNSIGNED=1` and a checksum file next to them.

Until the workflow and key exist, nothing in this repository claims releases are signed.

## Cutting a release

1. Bump `version` in the workspace `Cargo.toml`, run `mise run ci` (it includes the generated-docs
   drift test and the `vibeke/1` freeze check; see below).
2. `mise run repro-check` for the Linux artifacts (see [hardening.md](hardening.md)), then `mise run dist`; smoke-test `dist/<version>/vibeke-macos-aarch64 --version`.
3. Tag `v<version>`; the release workflow (when it exists) rebuilds from the tag, signs
   `SHA256SUMS`, and uploads the assets.
4. `scripts/install.sh` defaults to
   `https://github.com/MidgardAI/vibeke/releases/download/v<version>`; override with
   `VIBEKE_RELEASE_URL` for a mirror, or `VIBEKE_INSTALL_FROM=<dir>` for an offline install.

## API freeze and generated docs (M6 groundwork)

- `docs/api/methods.json` and `docs/api/README.md` are generated from the server's `METHODS`
  tables. `docs/api/vibeke-1.frozen.json` is the `vibeke/1` snapshot. Removing a frozen method or
  changing its mutating or scope flag fails `cargo test -p vibeke --test api_docs`; additions pass.
  **The freeze is a draft until 1.0**. Before a release, review the catalog diff, then refresh the
  snapshot with `VIBEKE_UPDATE_API_FREEZE=1 cargo test -p vibeke --test api_docs vibeke_1_freeze`
  (add `VIBEKE_API_FREEZE_ALLOW_BREAK=1` only for a justified break while the freeze is a draft).
- After adding a method, a CLI command or a config key, run
  `VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test api_docs` and commit the regenerated files
  (`docs/api/`, `docs/site/src/reference/`).
- `mise run docs` builds the docs site (`docs/site/`) when `mdbook` is installed.
