# Installation

Install the terminal CLI, download the desktop app, or open the [browser app](https://app.vibeke.dev).
Vibeke is pre-1.0 software. Public releases do not require a GitHub account.

## Terminal CLI

The host CLI runs on macOS with Apple silicon and Linux with x86_64 or aarch64 processors.
Linux binaries use static musl linking.

Install `minisign` first. It verifies the release signature. On macOS:

```sh
brew install minisign
```

On Debian or Ubuntu:

```sh
sudo apt install minisign
```

Then install Vibeke:

```sh
curl -fsSL https://vibeke.dev/install.sh | sh
```

The installer downloads the latest published release installer from GitHub. It verifies the signature and binary checksum before installing.
It installs inside your home directory without `sudo`.

Check your installation:

```sh
vibeke --version
vibeke doctor
```

If your shell cannot find Vibeke, add this line to your shell configuration and open a new terminal:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Continue with [your first workspace](quickstart.md).

## Desktop app

Download the installer for your computer from the [latest release](https://github.com/MidgardAI/vibeke/releases/latest).

| Platform | Download |
| --- | --- |
| macOS, Apple silicon | `Vibeke-<version>-mac-arm64.dmg` |
| macOS, Intel | `Vibeke-<version>-mac-x64.dmg` |
| Linux, x86_64 | `Vibeke-<version>-linux-x86_64.AppImage` or `Vibeke-<version>-linux-amd64.deb` |
| Windows, x86_64 | `Vibeke-<version>-win-x64.exe` |

On macOS, open the DMG and drag Vibeke into Applications. ZIP downloads are also available.

The desktop app connects to a Vibeke host. It does not bundle the host CLI.
For local use on a supported Mac or Linux computer, install the CLI too.
Windows and Intel Macs can connect to a supported remote host. This release has no host CLI for those platforms.

The macOS app is ad-hoc signed and is not notarized. The Windows installer is not publisher-signed.
Operating-system security checks can require approval. Desktop downloads are included in the signed release checksums.

Continue with the [desktop connection guide](desktop.md).

## Browser and phone

Open [app.vibeke.dev](https://app.vibeke.dev). Pair it with your host using the [phone and browser guide](mobile.md).
You do not need to build the app or run a relay.

## Upgrade

Run the installation command again to install the latest CLI release. Reconnect with `vibeke` afterward.
See [process durability](concepts/holders.md) for what survives a server restart.

For the desktop app, download and install the new release. Automatic desktop updates are not configured.

## Advanced installation

The CLI lives at `~/.local/share/vibeke/versions/<version>/vibeke`.
The installer creates links at `~/.local/share/vibeke/current` and `~/.local/bin/vibeke`.

To select a version, set the variable on the shell that runs the installer:

```sh
curl -fsSL https://vibeke.dev/install.sh | VIBEKE_VERSION=0.1.0 sh
```

For mirrors, offline installation, signatures, and development builds, see [release verification](reference/releases.md).
To contribute code, see [building from source](development.md).
