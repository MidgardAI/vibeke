# Work on a remote machine

This guide shows how to run Vibeke on a remote machine and use it from your terminal, the desktop app or your phone.
The agents run on the remote host. You see and control them from your computer.

## Connect from the terminal

Install the [CLI](install.md) on your computer. Make sure your usual SSH connection to the host works.
The host must run a supported platform:

- macOS 13 or later with Apple silicon or Intel processors.
- Linux with x86_64 or aarch64 processors.

### Install from signed releases

Add a machine to `~/.config/vibeke/config.toml`. Replace the address with your SSH host alias or address:

```toml
[[remote.machine]]
label = "devbox"
address = "devbox"
bootstrap = "remote-download"
```

Then connect:

```sh
vibeke ssh devbox
```

On the first connection, Vibeke shows the login prompts of `ssh`, such as a host key question, a password or a Tailscale sign-in link.

Vibeke downloads and verifies the signed release manifest on your computer.
The remote host downloads the matching binary and checks its checksum before installation.
The host needs `curl` and outbound access to GitHub release downloads.

When the host runs an older version, `vibeke ssh` asks if it should upgrade the host.
Press Enter or `y` to upgrade. Your panes keep running during the upgrade.
In scripts, which cannot answer the question, run `vibeke ssh devbox --upgrade`.
When the host runs a newer version, run `vibeke update` on your computer first.

### Use a local release cache

The default `bootstrap = "push"` mode uploads a verified binary from your local release cache.
Place the target host binary, `SHA256SUMS`, and `SHA256SUMS.minisig` together under `~/.cache/vibeke/releases/<version>/`.
Use the same release version as your local CLI.

See [release verification](reference/releases.md) for offline installation and development builds.

### Work on the host

The terminal connection passes through SSH. Use the sidebar to navigate the remote workspaces.
Agent commands run on the remote host and use its installed integrations and credentials.

Previews and forwarded ports use loopback addresses. See [previews and browsers](concepts/previews.md).
Use `vibeke machine list` and `vibeke machine status devbox` to inspect saved machines.

## Connect from the desktop app

The desktop app connects to a remote host through the public relay. The remote host needs no inbound port.

1. On the remote host, follow the [pairing instructions](mobile.md#pair-your-device).
2. Paste the resulting pairing link into the desktop app.
3. Confirm the device fingerprint on the host.

See the [desktop app guide](desktop.md) for the local connection and the menu bar.

## Preview apps on the remote host

A dev server that runs on the remote host appears in the **Previews** view.
With `vibeke ssh`, the browser runs on your computer and reaches the app through the SSH connection.
See [previews and browsers](concepts/previews.md#previews-over-vibeke-ssh).

## Connect from a phone

Pair your phone with the remote host from a shell on that host.
This works over SSH and needs no browser on the host. Follow [phone and browser access](mobile.md).

## Next steps

- [Phone and browser access](mobile.md): pair a phone with a host.
- [Previews and browsers](concepts/previews.md): open apps that run on the remote host.
- [Troubleshooting](troubleshooting.md): fix connection problems.
