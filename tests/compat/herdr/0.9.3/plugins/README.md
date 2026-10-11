# Herdr plugin corpus

These files choose the Herdr plugins that Vibeke tests against (spec 18 §6).
The corpus is the 20 highest-ranked eligible plugins. Rank is stars, then older creation date, then repository name.

## Files

- `catalog.json`: a reduced snapshot of the public plugin catalog. It keeps the first 40 eligible manifests that are not excluded, with only the fields that selection needs. It also records the time and sha256 of the full catalog. CI never reads the live catalog to choose plugins.
- `exclude.toml`: the inputs to the lock besides the snapshot.
  - `[[exclude]]` entries leave a repository out, keyed by numeric repository id. The next eligible plugin takes the slot.
  - `[[defer]]` entries keep a locked repository in its slot but do not run it for now. Each has a neutral reason. The lock carries the list. The workflow matrix leaves these plugins out, and the report lists them under "Deferred".
- `top20.lock.toml`: the generated lock. It holds the commit, manifest hash and toolchain versions. Do not edit it by hand.

## Commands

Run these from the repository root. They need `bun` and `git`.

- `bun scripts/herdr-plugins-lock.ts` writes the lock from the snapshot. It prints every moved pin and new entrant. Review them.
- `bun scripts/herdr-plugins-lock.ts --snapshot` replaces `catalog.json` with a reduced snapshot of the live catalog. A person runs this, then regenerates the lock.
- `bun scripts/herdr-plugins-lock.ts --check` compares the live catalog with the lock and prints a Markdown report. It never writes files.
- `bun scripts/herdr-plugins-fetch.ts --mirror <dir>` fetches each locked repository at its pinned commit into `<dir>/<owner>/<repo>`. Use `VIBEKE_PLUGIN_GIT_BASE=file://<dir>` to install from it. It never runs plugin code.
- `bun scripts/herdr-plugins-scan.ts --mirror <dir>` prints one JSON line per plugin. `raw_socket` is true when the source uses the Herdr socket directly instead of the `herdr` command.
- `bun test scripts/herdr-plugins` runs the unit tests.

## Screenshots

The run summary of the `herdr-plugins` workflow links to a PNG image of the plugin panes on Herdr and on Vibeke for each plugin job.
