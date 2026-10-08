# Build from source

Build from source when contributing or testing unreleased changes.

## Build the CLI

Install [mise](https://mise.jdx.dev/), then clone the public repository:

```sh
git clone https://github.com/MidgardAI/vibeke.git
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

## Web and desktop development

See the [web guide](../../../web/README.md) and [desktop development guide](../../../web/apps/desktop/README.md).
The product website reads its documentation directly from `docs/site/src/`.
Register new chapters in `SUMMARY.md` and `web/apps/site/content/manifest.ts`.

CLI, configuration, and API references are generated from source. Use the regeneration instructions in [releases](reference/releases.md).
