# Install

Supported hosts: macOS (Apple silicon) and Linux (x86_64 and aarch64, static musl binaries). Windows arrives with M6.

## Installer

```sh
curl -fsSL https://github.com/MidgardAI/vibeke/releases/latest/download/install.sh | sh
```

The script installs into `~/.local/share/vibeke/versions/<v>/vibeke`, points `~/.local/share/vibeke/current` at it and links `~/.local/bin/vibeke`. It never uses `sudo` and never writes outside `$HOME`. It verifies the binary against `SHA256SUMS` before installing. Until a release key exists, releases are not signed; see [Releases](reference/releases.md) for what is verified.

Environment: `VIBEKE_VERSION`, `VIBEKE_RELEASE_URL` (base URL holding `vibeke-<os>-<arch>` and `SHA256SUMS`) and `VIBEKE_INSTALL_FROM` (a local directory, for offline installs).

## From source

The toolchain (Rust, Zig, cargo-zigbuild) is pinned in `mise.toml`:

```sh
mise install
mise run build            # debug build of the workspace
mise run dist             # release binaries into dist/<version>/
```

## Checking the install

```sh
vibeke --version
vibeke doctor
```
