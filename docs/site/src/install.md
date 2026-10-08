# Installation

Install the terminal CLI, download the desktop app, or open the [browser app](https://app.vibeke.dev).

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

{{#include ../../desktop-downloads.md}}

See [all release files and notes](https://github.com/MidgardAI/vibeke/releases/latest).

On macOS, open the DMG and drag Vibeke into Applications. On Windows, run the EXE installer.
On Linux, choose the DEB for Debian-based systems or the AppImage for other distributions.
The Linux desktop app requires an available Secret Service/keyring backend.

The desktop app connects to a Vibeke host. It does not bundle the host CLI.
For local use on a supported Mac or Linux computer, install the CLI too.
Windows and Intel Macs can connect to a supported remote host. This release has no host CLI for those platforms.

The macOS app is ad-hoc signed and is not notarized. The Windows installer is not publisher-signed.
Operating-system security checks can require approval.

Continue with the [desktop connection guide](desktop.md).

## Browser and phone

Open [app.vibeke.dev](https://app.vibeke.dev). Pair it with your host using the [phone and browser guide](mobile.md).

## Update Vibeke

In the terminal interface, open the command palette with **Ctrl+B**, then **:** and choose
**Check for updates** or **Update Vibeke…**. When a release is available, the sidebar shows
its version. Review it and confirm installation; Vibeke reopens the same session and pane.
Background checks run every six hours and can be turned off in the update view.

You can also update from the shell:

```sh
vibeke update --check
vibeke update
```

The updater verifies the signed release manifest and binary checksum. Running terminal
processes survive the local session's restart. Other sessions use the installed version
on their next restart; remote hosts are updated separately. Use `vibeke update --rollback`
to return to the previous installed version.

For desktop updates, use **Check for Updates…** in the app menu. See the
[desktop guide](desktop.md) for in-app installation and manual download options.

## Advanced installation

The CLI lives at `~/.local/share/vibeke/versions/<version>/vibeke`.
The installer creates links at `~/.local/share/vibeke/current` and `~/.local/bin/vibeke`.

To select a version, set the variable on the shell that runs the installer:

```sh
curl -fsSL https://vibeke.dev/install.sh | VIBEKE_VERSION=0.2.0 sh
```

For mirrors, offline installation, signatures, and development builds, see [release verification](reference/releases.md).
To contribute code, see [building from source](development.md).
