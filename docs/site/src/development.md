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
Read [Verify a release](reference/releases.md) before you use these artifacts on a remote host.

## Web and desktop development

See the [web guide](../../../web/README.md) and [desktop development guide](../../../web/apps/desktop/README.md).
The product website reads its documentation directly from `docs/site/src/`.
Register new chapters in `SUMMARY.md` and `web/apps/site/content/manifest.ts`.

CLI, configuration, and API references are generated from source. After you change a method, a command or a setting, regenerate them:

```sh
VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test it api_docs::
```

Commit the changed files in `docs/api/` and `docs/site/src/reference/`. The repository also describes the release steps in [`docs/releases.md`](https://github.com/MidgardAI/vibeke/blob/main/docs/releases.md).
