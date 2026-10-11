# Verify a release

Every Vibeke release has a list of checksums and a signature. The signature proves that the Vibeke maintainers published the list. The checksums prove that your download matches the list.

The installer, `vibeke update`, `vibeke ssh` and the desktop updater make these checks for you. This page explains what they check. It also shows how to check a download yourself.

## What each path checks

| Path | What it checks |
| --- | --- |
| Installer (`install.sh`) | The signature of `SHA256SUMS`. Then the checksum of the binary. |
| `vibeke update` | A signed manifest that names the version, the download address and the checksum. |
| `vibeke ssh` and `vibeke machine upgrade` | The signature and the checksum of the binary before Vibeke uploads it. The remote host checks the checksum again before it switches versions. |
| Remote download | The signed manifest on your computer. The remote host then checks the checksum from the manifest. |
| Desktop app updates | The signature of `SHA256SUMS`, the version, and the hashes of the update files. The app also uses the signature checks of your operating system. |

Each check accepts a signature from the current key or the next key. The next key is published one release before it signs a release. This lets Vibeke change keys without a flag day.

## Check a download by hand

Install `minisign`. Download `SHA256SUMS`, `SHA256SUMS.minisig` and your file from the release page. Put them in one folder. Then run:

```sh
minisign -V -P RWQ0X1WPxwluX2gFO4vO586PSTdpSfJqrb+xsQnZ2ctND/VDw7VCWx5z -m SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing
```

The first command prints a trusted comment. It must name the version that you downloaded. The second command must report `OK` for your file.

The [installation page](../install.md#verify-a-release-by-hand) lists both public keys.

A checksum alone only finds a damaged download. An attacker who replaces both the file and its checksum list passes a checksum check. Only the signature shows that the maintainers published the list.

## Install without a network

For a host without internet access, copy the binary for its platform, `SHA256SUMS` and `SHA256SUMS.minisig` to one folder on that host. Then run the installer with that folder:

```sh
VIBEKE_INSTALL_FROM=/path/to/folder sh install.sh
```

For `vibeke ssh`, place the same three files in `~/.cache/vibeke/releases/<version>/` on your computer. See [Connect through SSH](../remote.md).

## Use a mirror

The default release address is `https://github.com/MidgardAI/vibeke/releases/download/v<version>`. Set `VIBEKE_RELEASE_URL` to use a mirror. The signature checks still apply to files from a mirror.

## Unsigned builds

If you build Vibeke yourself, the files have no signature. Set `VIBEKE_ALLOW_UNSIGNED=1` to accept them. Vibeke then prints a warning with the file name and its checksum. A checksum must still be present and correct. Online updates have no unsigned mode.

## Limits

- Vibeke does not publish build provenance from a transparency log yet.
- The macOS desktop app is signed with an Apple Developer ID and notarized. The Windows installer is not signed by a publisher, so Windows can ask you to approve it.
- The minisign signature and the operating system signature are separate checks.

Maintainers can read the release steps and key rotation plan in the [source repository](https://github.com/MidgardAI/vibeke/blob/main/docs/releases.md).
