# Herdr plugin corpus

These files choose the Herdr plugins that Vibeke tests against (spec 18 §6).
The corpus is the 20 highest-ranked eligible plugins. Rank is stars, then older creation date, then repository name.

## Files

- `catalog.json`: a snapshot of the public plugin catalog. CI never reads the live catalog to choose plugins.
- `exclude.toml`: repositories left out, keyed by numeric repository id. The next eligible plugin takes the slot.
- `top20.lock.toml`: the generated lock. It holds the commit, manifest hash and toolchain versions. Do not edit it by hand.

## Commands

Run these from the repository root. They need `bun` and `git`.

- `bun scripts/herdr-plugins-lock.ts` writes the lock from the snapshot. It prints every moved pin and new entrant. Review them.
- `bun scripts/herdr-plugins-lock.ts --snapshot` replaces `catalog.json` with the live catalog. A person runs this, then regenerates the lock.
- `bun scripts/herdr-plugins-lock.ts --check` compares the live catalog with the lock and prints a Markdown report. It never writes files.
- `bun scripts/herdr-plugins-fetch.ts --mirror <dir>` fetches each locked repository at its pinned commit into `<dir>/<owner>/<repo>`. Use `VIBEKE_PLUGIN_GIT_BASE=file://<dir>` to install from it. It never runs plugin code.
- `bun scripts/herdr-plugins-scan.ts --mirror <dir>` prints one JSON line per plugin. `raw_socket` is true when the source uses the Herdr socket directly instead of the `herdr` command.
- `bun test scripts/herdr-plugins` runs the unit tests.
