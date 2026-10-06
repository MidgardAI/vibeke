# Installation

Vibeke supports macOS on Apple silicon and Linux on x86_64 or aarch64. Linux release binaries use static musl linking. Windows support is planned.

Vibeke is pre-release software. Build from source to use the current code. Published releases contain the available installation files.

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

1. Check the [release files](https://github.com/MidgardAI/vibeke/releases).
2. If the release includes `install.sh`, run the installer:

   ```sh
   curl -fsSL https://github.com/MidgardAI/vibeke/releases/latest/download/install.sh | sh
   ```

The installer writes the binary to `~/.local/share/vibeke/versions/<v>/vibeke`. It creates links at `~/.local/share/vibeke/current` and `~/.local/bin/vibeke`.

The installer does not use `sudo`. It writes only inside `$HOME`. It checks the binary against `SHA256SUMS` before installation.

Release signatures are not available yet. See [release verification](reference/releases.md) for the limits of checksum checks.

| Variable | Purpose |
| --- | --- |
| `VIBEKE_VERSION` | Select the release version. |
| `VIBEKE_RELEASE_URL` | Set the base URL for binaries and `SHA256SUMS`. |
| `VIBEKE_INSTALL_FROM` | Use a local directory for offline installation. |

## Check the installation

```sh
vibeke --version
vibeke doctor
```

If the shell cannot find Vibeke, add `~/.local/bin` to `PATH`. Then use the [quickstart](quickstart.md) to create a workspace.
