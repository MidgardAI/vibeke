# Releases

## Release keys

Releases are signed with [minisign](https://jedisct1.github.io/minisign/). The binary embeds two public keys: the **current** key and the **next** key. The next key is embedded one release before its first use, so a rotation never needs a flag day.

| Label | Key id | File | Public key |
| --- | --- | --- | --- |
| current | `5F6E09C78F555F34` | [`keys/vibeke-2026.pub`](../keys/vibeke-2026.pub) | `RWQ0X1WPxwluX2gFO4vO586PSTdpSfJqrb+xsQnZ2ctND/VDw7VCWx5z` |
| next | `69536A23D04E2C7C` | [`keys/vibeke-next.pub`](../keys/vibeke-next.pub) | `RWR8LE7QI2pTaSsb4srEFbF1j78fXZzbORy4KGRzHErddJwSJLxwqH3x` |

The key id is the fingerprint minisign prints for a key. To check a download by hand:

```sh
minisign -V -P RWQ0X1WPxwluX2gFO4vO586PSTdpSfJqrb+xsQnZ2ctND/VDw7VCWx5z -m SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
```

The keys live in `vk_remote::bootstrap` (`KEY_CURRENT`, `KEY_NEXT`, `RELEASE_KEYS`) and in `scripts/install.sh`. A test (`scripts/tests/release-sign-test.sh`) checks that those two and `keys/*.pub` agree. Keep signing secrets outside the checkout and CI. Minisign reads the key locally and prompts for its password.

## What verification requires

Every release path requires a valid signature by the current or the next key. The error messages name both key ids.

| Path | What is verified |
| --- | --- |
| `scripts/install.sh` | `SHA256SUMS.minisig` over `SHA256SUMS` with the `minisign` tool (`brew install minisign`), then the binary against its `SHA256SUMS` entry. |
| `vibeke ssh`, `vibeke machine upgrade` (`bootstrap = "push"`) | A cached or `VIBEKE_ARTIFACT_DIR` binary: its `SHA256SUMS` entry and the signature, before upload. The remote re-checks the sha256 before the switch. |
| `bootstrap = "remote-download"` | The laptop downloads and verifies `manifest.json` and `manifest.json.minisig`. The signature must name `version:<v>` in its trusted comment and `<v>` must be this build's version. The remote then downloads the file and checks the manifest's sha256. |
| `vibeke update` | A cached or `--from` binary: checksum and signature before it is executed. |
| `vibeke integration update` (manifest channel) | `index.json.minisig` over the index, by the same two keys. The channel has no key of its own. |

### Unsigned development builds

`VIBEKE_ALLOW_UNSIGNED=1` accepts an artifact whose signature cannot be verified, with a warning that names the file and its SHA-256. It does not waive the checksum:

- The expected checksum must come from `SHA256SUMS` or a `<binary>.sha256` file next to the binary.
- A missing checksum file or a mismatch is always refused.
- The installer accepts the opt-in the same way, with `SHA256SUMS` from the download.
- `remote-download` has no unsigned mode, because there is no local file to checksum.
- The manifest channel keeps its separate development opt-in `VIBEKE_ALLOW_UNSIGNED_MANIFESTS=1`. The sha256 and serial checks still apply.

SSH can upload the running binary when the host platforms match, but only with `VIBEKE_ALLOW_UNSIGNED=1`. Without an external checksum, its hash protects only the transfer.

The remote host checks the uploaded file before it changes `current`. On mismatch, it preserves the previous version. Where `mv -T` is available, the change uses an atomic rename:

```sh
ln -s versions/<v> current.new && mv -Tf current.new current
```

Other systems use `ln -sfn`, which is not atomic. For cached updates, the directory name supplies the version. With `--from`, the verified binary supplies it.

Checksums alone detect corruption. They do not prove who built the file: an attacker who replaces both files passes an unsigned check. The signature is what proves the maintainer's key signed the checksum list.

## Release repository and tokens

The default release URL is `https://github.com/MidgardAI/vibeke/releases/download/v<version>`. Set `VIBEKE_RELEASE_URL` for a mirror, or `VIBEKE_INSTALL_FROM=<dir>` for offline installation.

While the repository is **private**, GitHub does not serve the plain download URL without authentication. The installer and `remote-download` therefore honour `VIBEKE_GITHUB_TOKEN`, else `GITHUB_TOKEN`:

- With a token and a `github.com/<owner>/<repo>/releases/download/...` URL, the file is fetched through the GitHub API asset endpoint (`/repos/<owner>/<repo>/releases/assets/<id>`) with `Accept: application/octet-stream` and `Authorization: Bearer <token>`.
- curl reads the headers from stdin, so the token is never on a command line (`ps`). Nothing prints it. Error messages say only that a token is needed.
- The token is sent only to `github.com` release URLs and the GitHub API, never to a mirror in `VIBEKE_RELEASE_URL`. curl drops it when GitHub redirects to its storage host.
- For `remote-download` the laptop resolves the asset URL. The remote then runs `curl` with the headers from a here-document on the stdin of `sh -s`, so the token is not visible in the remote's process list or shell history either.
- A fine-grained token with read access to the repository's contents is enough.

Public releases need no token. `VIBEKE_GITHUB_API_URL` overrides the API base (tests, GitHub Enterprise).

## Build release files locally

`scripts/release-build.sh <version>` builds the artifacts reproducibly into `dist/<version>/`. It checks that `<version>` matches the workspace `Cargo.toml`, builds with `SOURCE_DATE_EPOCH` from the commit, `--remap-path-prefix`, `--locked` and stripped symbols (the environment `scripts/repro-check.sh` uses), and writes a `.sha256` file per binary plus `SHA256SUMS`. It signs and publishes nothing.

| Artifact | Target triple | Built by |
| --- | --- | --- |
| `vibeke-macos-aarch64` | `aarch64-apple-darwin` | `cargo build --release`, on an Apple-silicon Mac |
| `vibeke-linux-x86_64` | `x86_64-unknown-linux-musl` | `cargo zigbuild --release` |
| `vibeke-linux-aarch64` | `aarch64-unknown-linux-musl` | `cargo zigbuild --release` |

A target whose toolchain is missing is skipped with a notice. Use `mise exec -- sh scripts/release-build.sh <version>` so Zig and cargo-zigbuild are on `PATH`. `--repro-check` first runs `scripts/repro-check.sh` for both Linux targets. `--only macos|linux` builds one family.

Linux binaries use static musl linking. Zig 0.16 provides the C toolchain for libghostty-vt and linking. `mise.toml` selects the Zig version.

`mise run dist` (`scripts/dist.sh`) is the development variant. It also copies the files to `~/.cache/vibeke/releases/<version>/`, which SSH installation and `vibeke update` read. Set `VIBEKE_RELEASES_DIR` to use another cache directory. Its output is unsigned, so using it needs `VIBEKE_ALLOW_UNSIGNED=1` or a signed `SHA256SUMS`.

## Release steps

Signing happens locally. CI never holds a signing secret.

1. Update the workspace version in `Cargo.toml` and commit it.
2. Run `mise run ci`, then `mise run repro-check` (see the [hardening guide](hardening.md)).
3. Tag and push: `git tag v<version> && git push origin v<version>`.
4. The `release` workflow (`.github/workflows/release.yml`) builds the macOS and Linux artifacts, verifies that all three binaries match their `.sha256` files, and creates a **draft** release `v<version>` with those files and an `install.sh` pinned to the tag's version. It needs no secret beyond the repository's own `GITHUB_TOKEN`.
5. Download the draft's files into one directory, for example with `gh release download v<version> --dir dist/<version>` (a private repository needs `gh auth login`). Alternatively build locally with `scripts/release-build.sh <version>` and use `dist/<version>/`. If you built both, compare the sha256 of the files first: they should be identical.
6. Sign: `scripts/release-sign.sh dist/<version>`. The script writes `SHA256SUMS` and `manifest.json`, then runs minisign twice (`minisign -S -s ~/.vibeke-release-keys/vibeke-2026.key -m <file> -t "vibeke v<version>"`). Enter the key password when minisign asks. The script finishes by verifying both signatures with the public key embedded in the binary, and fails if they do not verify.
7. Upload the four signing outputs to the draft: `gh release upload v<version> dist/<version>/SHA256SUMS dist/<version>/SHA256SUMS.minisig dist/<version>/manifest.json dist/<version>/manifest.json.minisig`. The workflow already includes the version-pinned installer.
8. Check the result with a fresh checkout of the files: `minisign -V -P <public key> -m SHA256SUMS`.
9. Publish the draft (`gh release edit v<version> --draft=false`).
10. Smoke test: run `scripts/install.sh` with `VIBEKE_VERSION=<version>` (and `GITHUB_TOKEN` while the repository is private) on a clean `HOME`.

`manifest.json` lists `{version, artifacts: [{target, sha256, url}]}`. Its signature carries the trusted comment `vibeke v<version> version:<version>`, so an old manifest cannot be replayed under a new version.

## Key rotation

The binary embeds the current and the next key. To rotate:

1. Release N is signed with the **current** key and already embeds the **next** key. Binaries from release N accept signatures by either key.
2. Release N+1 is signed with the **next** key: `scripts/release-sign.sh --key next dist/<version>` (it uses `~/.vibeke-release-keys/vibeke-next.key` and verifies against `KEY_NEXT`). Binaries from release N accept it, so they can update to N+1.
3. In release N+1 (or later), promote the keys in `vk_remote::bootstrap` and `scripts/install.sh`: the old next key becomes the current key, and a newly generated key becomes next. Update `keys/` and the tables here.
4. Announce each rotation one release ahead.

If a key is lost or exposed, publish the replacement through a separate trusted channel and state the first version that uses it. Binaries that embed only the exposed key need a manual reinstall.

## Not provided

- Sigstore build provenance. Spec 09 §10 lists it as a later addition.
- An Apple Developer ID: the macOS binary is not notarized, so Gatekeeper may warn when it is downloaded in a browser. A binary fetched by `curl` or the installer carries no quarantine flag.

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
