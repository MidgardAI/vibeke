# Connect through SSH

Install the [CLI](install.md) on your computer. Make sure your usual SSH connection to the host works.
The host must run a supported platform: Apple silicon macOS, or x86_64/aarch64 Linux.

## Install from signed releases

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

Vibeke downloads and verifies the signed release manifest on your computer.
The remote host downloads the matching binary and checks its checksum before installation.
The host needs `curl` and outbound access to GitHub release downloads.
Public releases need no GitHub token or unsigned-build flag.

Vibeke installs inside the remote user's home directory without `sudo`.
For an existing installation that needs an upgrade, review the version change and run `vibeke ssh devbox --upgrade`.

## Use a local release cache

The default `bootstrap = "push"` mode uploads a verified binary from your local release cache.
Place the target host binary, `SHA256SUMS`, and `SHA256SUMS.minisig` together under `~/.cache/vibeke/releases/<version>/`.
Use the same release version as your local CLI.

See [release verification](reference/releases.md) for offline installation and development builds.

## Work on the host

The terminal connection passes through SSH. Use the sidebar to navigate the remote workspaces.
Agent commands run on the remote host and use its installed integrations and credentials.

Previews and forwarded ports use loopback addresses. See [previews and browsers](concepts/previews.md).
Use `vibeke machine list` and `vibeke machine status devbox` to inspect saved machines.
