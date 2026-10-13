# Build from source

Build from source when contributing or testing unreleased changes.

## Build the CLI

Install [mise](https://mise.jdx.dev/), then clone the public repository:

```sh
git clone https://github.com/vibeke-ai/vibeke.git
cd vibeke
mise install
mise run build
```

Use the debug binary in this shell:

```sh
export PATH="$PWD/target/debug:$PATH"
vibeke --version
vibeke doctor
```

Run `mise run test` for tests and `mise run ci` for the full local checks.
`mise run dist` creates unsigned development artifacts and updates the local remote-install cache.
See [releases and reproducible builds](reference/releases.md) before using these artifacts remotely.

## Measure terminal performance

Run the native terminal latency probe from a source build:

```sh
vibeke debug tui-latency --n 200 --json
```

The probe starts an isolated server and the native TUI in a pseudo-terminal.
It measures the time from a key to visible output through the full client path.
It reports latency percentiles and changes in render counters.
Compare runs from the same build profile on the same machine.
Use `--binary /path/to/another/vibeke` to measure another build with the same probe.

`vibeke server status --json` includes render wakeups, timer wakeups, row projections,
and pane frame counts under `render`.
Row projections count full visible-row conversions made by render sessions.
Other readers can populate the shared row cache first.

## Web and desktop development

See the [web guide](../../../web/README.md) and [desktop development guide](../../../web/apps/desktop/README.md).
The product website reads its documentation directly from `docs/site/src/`.
Register new chapters in `SUMMARY.md` and `web/apps/site/content/manifest.ts`.

CLI, configuration, and API references are generated from source. Use the regeneration instructions in [releases](reference/releases.md).

## Next steps

- [CLI reference](reference/cli.md) for the commands you can test.
- [Release verification](reference/releases.md) before you use development artifacts remotely.
- [Troubleshooting](troubleshooting.md) for how to collect a debug bundle.
