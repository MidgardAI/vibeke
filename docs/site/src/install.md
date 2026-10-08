# Installation

Vibeke supports macOS on Apple silicon and Linux on x86_64 or aarch64. Linux release binaries use static musl linking. Windows support is planned.

Vibeke is pre-1.0 software. Install a tagged release for a fixed version, or build from source to use the current code.

## Build from source

1. Install [mise](https://mise.jdx.dev/).
2. Open the repository root in your shell.
3. Install the toolchains from `mise.toml`:

   ```sh
   mise install
   ```

4. Build Vibeke:

   ```sh
   mise run build
   ```

5. Add the debug binary directory to this shell's `PATH`:

   ```sh
   export PATH="$PWD/target/debug:$PATH"
   ```

6. Check the installation:

   ```sh
   vibeke --version
   vibeke doctor
   ```

For release binaries, use `mise run dist`. This command writes binaries and checksums to `dist/<version>/`. It also updates the local release cache for remote installation.

See [releases and reproducible builds](reference/releases.md).

## Install a published release

Check the [release files](https://github.com/MidgardAI/vibeke/releases). While the repository is private, use an authenticated GitHub CLI to download the release, then install the verified local files:

```sh
gh auth login
release_dir=$(mktemp -d)
gh release download v0.1.0 --repo MidgardAI/vibeke --dir "$release_dir" \
  --pattern 'vibeke-*' --pattern 'SHA256SUMS*' --pattern 'install.sh'
VIBEKE_INSTALL_FROM="$release_dir" sh "$release_dir/install.sh"
```

Once the repository is public, the same installer can be downloaded without authentication:

```sh
curl -fsSL https://github.com/MidgardAI/vibeke/releases/latest/download/install.sh | sh
```

The installer writes the binary to `~/.local/share/vibeke/versions/<v>/vibeke`. It creates links at `~/.local/share/vibeke/current` and `~/.local/bin/vibeke`.

The installer does not use `sudo`. It writes only inside `$HOME`. It verifies the minisign signature of `SHA256SUMS` against the embedded release keys, then checks the binary against `SHA256SUMS`, before it installs anything. This needs the `minisign` tool (`brew install minisign`). The release keys and their key ids are listed in [release verification](reference/releases.md).

For online installation from an already downloaded installer while the repository is private, set `GITHUB_TOKEN` (or `VIBEKE_GITHUB_TOKEN`) to a token with read access. The installer then downloads through the GitHub API and never prints the token. This does not authenticate the initial `curl` that fetches the installer; use the GitHub CLI flow above. Public releases need no token.

| Variable | Purpose |
| --- | --- |
| `VIBEKE_VERSION` | Select the release version. |
| `VIBEKE_RELEASE_URL` | Set the base URL for binaries, `SHA256SUMS` and `SHA256SUMS.minisig`. |
| `GITHUB_TOKEN`, `VIBEKE_GITHUB_TOKEN` | Read a private release repository. Not needed for public releases. |
| `VIBEKE_ALLOW_UNSIGNED` | Set to `1` to accept an unsigned release for development. The checksum must still match. |
| `VIBEKE_INSTALL_FROM` | Use a local directory for offline installation. |

## Desktop app

Download the desktop installer from the [release page](https://github.com/MidgardAI/vibeke/releases) while signed into GitHub with repository access:

| Platform | Download |
| --- | --- |
| macOS, Apple silicon | `Vibeke-<version>-mac-arm64.dmg` |
| macOS, Intel | `Vibeke-<version>-mac-x64.dmg` |
| Linux, x86_64 | `Vibeke-<version>-linux-x64.AppImage` or `.deb` |
| Windows, x86_64 | `Vibeke-<version>-win-x64.exe` |

On macOS, open the DMG and drag Vibeke into Applications. ZIP downloads are also available.

The desktop app connects to a Vibeke host; it does not bundle the host CLI. To use your own Mac or Linux machine as a host, install the CLI above as well. Intel Macs and Windows can use the desktop app to connect to a supported remote host; this release does not include a host binary for those platforms.

Desktop downloads are included in the signed `SHA256SUMS`. The macOS app is ad-hoc signed, without Apple Developer ID signing or notarization; the Windows installer is not publisher-signed. Operating-system security checks may require explicit approval. Automatic desktop updates are not configured; download a new release to upgrade.

## Check the installation

```sh
vibeke --version
vibeke doctor
```

If the shell cannot find Vibeke, add `~/.local/bin` to `PATH`. Then use the [quickstart](quickstart.md) to create a workspace.
