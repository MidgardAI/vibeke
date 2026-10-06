# Releases

## Build release files

`mise run dist` runs `scripts/dist.sh`. It builds the three targets below and writes files to `dist/<version>/`. The version comes from the workspace `Cargo.toml`.

| Artifact | Target triple | Build command |
| --- | --- | --- |
| `vibeke-macos-aarch64` | `aarch64-apple-darwin` | `cargo build --release` |
| `vibeke-linux-x86_64` | `x86_64-unknown-linux-musl` | `cargo zigbuild --release` |
| `vibeke-linux-aarch64` | `aarch64-unknown-linux-musl` | `cargo zigbuild --release` |

Each binary has a `<name>.sha256` file. `SHA256SUMS` lists all three checksums.

The build also copies these files to `~/.cache/vibeke/releases/<version>/`. SSH installation and local updates use this cache. Set `VIBEKE_RELEASES_DIR` to use another cache directory.

Linux binaries use static musl linking. Zig 0.16 provides the C toolchain for libghostty-vt and linking. `mise.toml` selects the Zig version.

## Current verification

No trusted release key exists yet. Releases are unsigned. `vk_remote::bootstrap::TRUSTED_KEYS` is empty, and `verify_signature()` returns `NoTrustedKeys`.

### Checksums

The expected checksum comes from `SHA256SUMS` beside the binary. If that entry is unavailable, verification uses `<binary>.sha256`.

A missing checksum file causes rejection. `VIBEKE_ALLOW_UNSIGNED=1` does not remove this requirement. The actual SHA-256 must match the expected value.

### Signatures

Verification accepts `SHA256SUMS.minisig` only with a trusted embedded key. Until a key exists, set `VIBEKE_ALLOW_UNSIGNED=1` to permit unsigned files.

The unsigned path prints the file name and SHA-256 as a warning. A separate `.sha256` file has no signature, so it always needs this option.

### Commands that verify files

| Command | Behavior |
| --- | --- |
| `vibeke ssh` | Checks a cached binary or `VIBEKE_ARTIFACT_DIR` file before upload. Rejects files that fail verification. |
| `vibeke update` | Checks a cached or `--from` binary before execution. Reads the version only after verification. |
| `scripts/install.sh` | Checks the downloaded binary against `SHA256SUMS` before installation. |

SSH can upload the current binary when host platforms match. This requires `VIBEKE_ALLOW_UNSIGNED=1`. Without an external checksum, its hash protects only the transfer.

The remote host checks the uploaded file before it changes `current`. On mismatch, it preserves the previous version.

Where `mv -T` is available, the change uses an atomic rename:

```sh
ln -s versions/<v> current.new && mv -Tf current.new current
```

Other systems use `ln -sfn`, which is not atomic.

For cached updates, the directory name supplies the version. With `--from`, the verified binary supplies it.

Checksums detect file corruption or a replaced binary beside an unchanged checksum. They do not prove who built the file. An attacker who replaces both files can pass unsigned verification.

## Planned release signing

The planned release workflow signs `SHA256SUMS` with [minisign](https://jedisct1.github.io/minisign/). It publishes the binaries, `SHA256SUMS`, and `SHA256SUMS.minisig` as release files.

The proposed key controls are:

- Generate the key pair offline. Keep the secret key outside the repository.
- Store the protected secret key and password in GitHub Actions secrets.
- Limit access to protected release tags and an environment with maintainer approval.
- Commit the public key and embed it in the installer and binary.

For normal key rotation, publish the new key in a release signed with the old key. Then change the release workflow key.

If a key is lost or exposed, publish a replacement through a separate trusted channel. State the first version that uses it.

After implementation, downloaded releases will require a valid signature. Local unsigned builds will still need an explicit option and checksum file.

The current repository does not provide this signing workflow.

## Prepare a release

1. Update the workspace version in `Cargo.toml`.
2. Run `mise run ci`.
3. Run `mise run repro-check` for Linux files. See the [hardening guide](hardening.md).
4. Run `mise run dist`.
5. Check `dist/<version>/vibeke-macos-aarch64 --version`.
6. Create the `v<version>` tag.

The planned release workflow will rebuild from the tag, sign the checksums, and publish the files.

The installer defaults to `https://github.com/MidgardAI/vibeke/releases/download/v<version>`. Set `VIBEKE_RELEASE_URL` for a mirror. Set `VIBEKE_INSTALL_FROM=<dir>` for offline installation.

## API compatibility and generated references

The server method tables generate `docs/api/methods.json` and `docs/api/README.md`. `docs/api/vibeke-1.frozen.json` records protected methods and access flags.

The compatibility test rejects removals or changes to protected access flags. Additions pass. The policy remains a draft until version 1.0.

Before a release, review the catalog changes. Then update the snapshot:

```sh
VIBEKE_UPDATE_API_FREEZE=1 cargo test -p vibeke --test api_docs vibeke_1_freeze
```

Use `VIBEKE_API_FREEZE_ALLOW_BREAK=1` only for a deliberate compatibility change before version 1.0.

After a method, command, or configuration change, regenerate the reference:

```sh
VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test api_docs
```

Commit the generated files in `docs/api/` and `docs/site/src/reference/`.

`mise run docs` builds the mdBook site when `mdbook` is installed.
