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

Checksums only. They catch corruption and a wrong or truncated download; they do not prove who
built the file, because anyone who can replace the binary can replace `SHA256SUMS` next to it.
Consumers verify like this:

- `scripts/install.sh` verifies the downloaded binary against `SHA256SUMS` before installing.
- `vibeke ssh` verifies the artifact locally, uploads it, and re-checks the sha256 on the remote
  before the atomic switch of the `current` symlink.
- `vibeke update` verifies the `.sha256` sidecar when one exists.

Builds made on your own machine are trusted by their local checksum.

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
signature keep working through the sidecar check.

Until the workflow and key exist, nothing in this repository claims releases are signed.

## Cutting a release

1. Bump `version` in the workspace `Cargo.toml`, run `mise run ci`.
2. `mise run dist`; smoke-test `dist/<version>/vibeke-macos-aarch64 --version`.
3. Tag `v<version>`; the release workflow (when it exists) rebuilds from the tag, signs
   `SHA256SUMS`, and uploads the assets.
4. `scripts/install.sh` defaults to
   `https://github.com/MidgardAI/vibeke/releases/download/v<version>`; override with
   `VIBEKE_RELEASE_URL` for a mirror, or `VIBEKE_INSTALL_FROM=<dir>` for an offline install.
