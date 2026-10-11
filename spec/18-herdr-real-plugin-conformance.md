# 18 — Real Herdr plugin conformance harness

**Status: built; first CI runs pending.** The code, the lock and 20 scenarios exist. Plugin results come only from the `herdr-plugins.yml` workflow (§12). This spec adds a harness that runs popular Herdr plugins, unchanged, against Vibeke's compatibility layer. The harness also compares what each plugin draws in its pane in Vibeke with what it draws in the pinned Herdr baseline. It implements part of [07](07-api-cli-plugins.md) §8.4 item 2 ("unmodified plugins") and [10](10-quality-performance-testing.md) §4.8.

The harness is a regression signal. It is not the M5 certification gate. A sample of popular plugins cannot prove full compatibility (07 §8.0). The full inventory and the differential suite stay required.

## 1. Terms

- **Corpus:** the set of real plugins the harness runs.
- **Scenario:** a hand-written list of steps and expected results for one plugin.
- **Broker:** the per-invocation socket server that answers a plugin's Herdr calls with the plugin's own authority (07 §7.7).
- **Execution boundary:** a disposable machine or virtual machine that runs one plugin and is destroyed afterwards.
- **Normalizer:** code that replaces values that change on every run (generated ids, timestamps, temp paths) before two outputs are compared.
- **Cell grid:** a terminal screen as rows of cells. Each cell has text, colors and attributes such as bold.

## 2. Goals

- Install the highest-ranked eligible Herdr plugins (§6), at pinned commits, with unchanged manifests and source.
- Run the actions, event hooks, pane placements, startup commands and link handlers that each plugin declares. Check the specific effect of each step.
- Give every result an outcome (§8.4), so a failure points at Vibeke, at a dependency, at the harness or at a step that did not run.
- Compare the plugin's pane contents in Herdr 0.9.3 and in Vibeke, and show both side by side.
- Never run third-party plugin code on a developer machine, next to trusted tooling, or with repository secrets.

## 3. Non-goals

- Comparing whole screens. Herdr and Vibeke draw different frames, sidebars and tab bars. Running Herdr's own TUI against Vibeke is out of scope (07 §8.0). The harness compares only the plugin's pane.
- Pixel comparison. The harness compares cell grids. Images exist only for people to look at.
- Certifying compatibility, or widening the advertised baseline.
- Windows. It joins with M6.

## 4. Current state

| Piece | Where | State |
|---|---|---|
| 99 real manifests, pinned by commit and sha256 | `tests/compat/herdr/0.9.3/manifests/` | Checked without running: sha256, ids, actions, events, qualified action names and platforms (`crates/vk-compat/tests/herdr_manifests.rs`). |
| Plugin system tests | `crates/vibeke/tests/it/compat_herdr_plugins_ext.rs` | Synthetic plugins, installed from `file://` Git mirrors. The file has its own private `Session` and helpers. |
| Differential harness | `crates/vibeke/tests/it/compat_herdr_diff.rs` | CLI scenarios only. It is a normal `#[test]` that returns early unless `VIBEKE_HERDR_DIFF=1`. It runs `--version` before its optional sha256 check. It has never run. |
| Baseline captures | `tests/compat/herdr/0.9.3/baseline.toml` | Binary checksums, API schema and CLI help are `pending`. |
| Callback audit | `plugin.api_call` in `crates/vk-server/src/compat.rs` | Records plugin, invocation, method, success and error code. Only mutating calls, and no params. Tests enforce this. |
| Pane screenshots | `pane.screenshot` in `crates/vk-server/src/pane_api.rs` | Text, ANSI, HTML, SVG and PNG. The ANSI form trims trailing blanks and merges underline styles, so it is not a full cell grid. |
| Generated inventory | `docs/herdr-compat-inventory.md`, from `vk_compat::herdr::inventory::render_markdown` | A test checks that the file equals the generated text. |

The plugin catalog (`https://assets.herdr.dev/plugins/index.json`) lists `stars` and `headCommit` per repository, and `platforms` per manifest. One repository can contain several manifests.

The plugins near the top need these surfaces:

- Build steps that download moving binaries, including `curl … | bash`.
- Toolchains: cargo, go, bun, node and npm.
- Overlay, split, tab and popup pane placements.
- `[[startup]]` commands and `link_handlers`.
- Events: `pane.agent_status_changed`, `pane.agent_detected`, `worktree.created`, `worktree.opened`, `tab.*`, `workspace.*`, `pane.created` and `pane.focused`.
- Some plugins declare no actions. One has only event hooks. One has only panes.

## 5. Security and isolation

Legacy plugin trust means arbitrary code execution. Cleaning HOME and the environment does not stop a plugin from reading runner files, changing the harness or leaving processes behind. The harness therefore uses an execution boundary per plugin:

1. Each plugin runs in its own fresh runner job or virtual machine. The boundary is destroyed after the job. No two plugins share a boundary.
2. The trusted parts stay outside the boundary or are read-only inside it: the `vibeke` binary, the test binary, the scenario files and the result checker. The job builds them in an earlier trusted job and passes them in as artifacts with checksums.
3. Inside the boundary, each plugin also gets a fresh HOME, XDG directories, TMPDIR and runtime directory. The environment is an allowlist. It contains no host credentials, `SSH_AUTH_SOCK`, cloud keys or GitHub tokens.
4. Each step has a timeout. Process-group cleanup is best effort only. Destroying the boundary is the real cleanup.
5. The plugin job has `permissions: contents: read`, checks out with `persist-credentials: false`, and receives no secrets. Its caches use their own key namespace. It never writes to the Rust caches that `weekly.yml` and `nightly.yml` share.
6. A separate reporting job reads the result artifacts. It treats them as hostile data: it checks their schema and size, escapes all text, and never runs anything they contain. Only this job may open or update issues.
7. The harness redacts text grids and traces with `vk-redact` before it renders any image. `vk-redact` cannot clean an image after the fact.

Trust model of the result files:

- The test process that writes a result runs as the same user as the plugin code.
- A malicious plugin at a locked commit could therefore fabricate step outcomes that are consistent with its scenario.
- The reporting job cross-checks every result against the trusted scenario files and the lock.
- It requires the plugin id, platform and commit from the lock, exactly the scenario's steps for that platform, the scenario's skipped surfaces, and the fixed lifecycle checks (`install` and `vibeke_side_stopped`).
- It rejects a result that does not match as a `harness_error`, which fails the run.
- It also fails a plugin whose test exit status, recorded outside the boundary, is not zero.
- The run is informational. It is a regression signal, not a security gate.
- Plugin commits are pinned in the lock. A change to a pin is a reviewed change.

## 6. Corpus selection

The lock file `tests/compat/herdr/0.9.3/plugins/top20.lock.toml` is generated by `scripts/herdr-plugins-lock.ts` (bun).

- **Input.** A checked-in catalog snapshot, with its sha256 and `generatedAt`. CI never reads the live catalog to choose plugins.
- **Unit.** One entry is one manifest. A repository with several manifests adds each eligible manifest. All of its manifests share the repository's star count.
- **Eligible.** The manifest's `platforms` include `macos` or `linux`. A manifest without `platforms` counts as all platforms, as in `vk_compat::herdr::manifest`. The baseline manifest schema confirms this rule when it is captured.
- **Excluded.** The maintainer keeps an exclusion list keyed by the catalog's numeric repository `id`, with the repository name and a neutral reason. The list starts with one entry:

  ```toml
  [[exclude]]
  id = 1283376338
  repository = "AltanS/collie"
  reason = "Vibeke has its own browser and phone app (PWA, a web app that installs like a native app), which covers this plugin's purpose."
  ```

  The next eligible plugin takes its place, so the corpus still has 20 entries.
- **Deferred.** The maintainer can also defer a locked repository for now. A deferral is a `[[defer]]` entry in the same input file, `exclude.toml`, with the repository name and a neutral reason. The lock script checks that each deferral names a locked repository, and it copies the list into the lock. A deferred plugin keeps its rank slot, so the next eligible plugin does **not** take its place. The workflow matrix leaves deferred plugins out, and the report lists them in a "Deferred" section with their reasons. Removing the entry runs the plugin again. The current deferrals:

  | Repository | Reason |
  |---|---|
  | `openclaw/crabbox` | It needs a Crabbox account and its own `crabbox` command. |
  | `zenbu-labs/terminal-browser` | It needs a browser that registers its terminal images with `pane.graphics.info`, which is newer than Herdr 0.9.3. |
  | `zenbu-labs/terminal-code` | The same browser requirement. |
  | `powerfooI/roamgate` | It needs a user service manager. |
  | `devswha/herdr-web-ui` | An action starts a background server. Vibeke closes an action's socket broker when the action exits, which is a documented difference. |
- **Order.** Stars, highest first. Ties go to the older `createdAt`, and then to the repository name.
- **Size.** The first 20 eligible, non-excluded manifests. Documents and reports call this "the 20 highest-ranked eligible plugins", not "the top 20".
- **Fields.** Repository, numeric id, plugin id, manifest path, commit, manifest sha256, stars at the snapshot.
- **Existing pins.** The lock reuses the pins in `manifests/index.json` when the commit matches. Moving a pin is a reviewed change to the lock.
- **Drift.** A weekly `--check` mode compares the live catalog and reports new entrants and new commits. It never edits the lock.

## 7. Installation and dependencies

### 7.1 Installation

- `scripts/herdr-plugins-fetch.ts` mirrors each repository at its pinned commit into `<mirror>/<owner>/<repo>`.
- The test installs `owner/repo[/subdir]@<sha>`, with `VIBEKE_TEST_HOOKS=1` and `VIBEKE_PLUGIN_GIT_BASE=file://<mirror>`. This is the real Git install path (`crates/vk-compat/src/herdr/source.rs`), and `compat_herdr_plugins_ext.rs` already uses it.
- After install, the test checks the checkout commit and the manifest sha256 against the lock.

### 7.2 Dependencies

Unchanged build scripts download code and run it straight away, for example with `curl … | bash`. The harness cannot check those bytes before they run. So each plugin gets one of two dependency modes in the lock:

- **`verified`:** all network access goes through a recording proxy with a content cache. The proxy serves only responses whose sha256 is in the lock. Any other request fails the build before the bytes reach the plugin. A first, separate "record" run fills the lock and the cache. A person reviews that change.
- **`unpinned`:** the plugin downloads freely. Its results are labeled non-reproducible in every report. The attribution rule in §8.4 applies only when both sides received the same dependency bytes.

The proxy covers build steps, runtime self-updates and transitive package downloads (npm, cargo, go modules). Toolchain versions (node, bun, go, rust) are part of the lock. `mise.toml` does not pin Node or Go today, so the harness workflow pins them.

## 8. Scenarios

Each plugin has a scenario at `tests/compat/herdr/0.9.3/plugins/scenarios/<owner>__<repo>.toml`. Calling every manifest action blindly gives false failures. For example, a file picker needs a handoff file, and an `open-link` action must arrive through a link handler.

A scenario contains:

- **Setup.** Repositories, worktrees, files, and a fake agent for agent status events.
- **Steps, in order.** An action with its context, an event trigger, a link to open, a server restart or a lifecycle command.
- **Expectations** for each step (§8.2).
- **Stubs** for external services and credentials only, such as chat bots, tunnels or cloud machines. The harness never stubs Herdr callbacks or plugin code.
- **Expected calls.** The Herdr calls the step should make, including calls that should fail. Error checks belong to the step. The harness does not reject every error.
- **Known results.** A step that cannot pass yet lists its expected outcome and a reason.
- **Platforms.** A step, a known result and a skip can name the platforms they apply to (`platforms = ["linux"]`). Without the field they apply everywhere. The coverage check (§8.1) counts a step or a skip only on its platforms, so a surface can be skipped on Linux and covered on macOS.
- **Context.** An action runs with a context: `global`, `workspace`, `tab`, `pane`, or `agent_pane`, which makes the fake agent the focused pane. `setup.workspace_cwd` starts the context workspace in a directory, such as a repository the setup creates. `setup.path_exclude` keeps directories off the plugin PATH on both sides.

### 8.1 Coverage check

A checker compares each scenario with its manifest. Every declared action, event, pane, startup command and link handler for the current platform must appear in a step. Otherwise it must appear in an explicit `skip` list with a reason. A missing entry fails the run. This stops a scenario from quietly covering less than the plugin declares.

### 8.2 Evidence that a step ran

No errors does not mean success. Every step needs positive evidence, such as:

- The invocation reaches a final log state (completed or failed), not only "launched".
- The expected pane appears at the declared placement.
- A popup appears. A popup result has `pane: null`, so the harness finds the popup through the popup surface, not through `pane.screenshot`.
- The pane screen contains the expected text once it is stable.
- An expected state file or event-hook log line appears.
- A long-running startup command reports that it is ready before dependent steps run.

### 8.3 Lifecycle checks

Each is its own assertion:

- Install, trust and build succeed.
- Link and unlink work.
- Startup commands run before dependent actions.
- Startup commands run exactly once per activation.
- Enabling a plugin does not run startup commands.
- Reinstall keeps the plugin's config and state.
- A disabled plugin's callbacks are rejected.
- A revoked plugin's callbacks are rejected.
- Uninstall removes managed files and the registration.

Uninstall does not stop a plugin's own background services. Closing the broker only ends its connections. The scenario stops those services itself.

### 8.4 Outcomes

| Outcome | Meaning |
|---|---|
| `passed` | All expectations held. |
| `suspected_compat_failure` | Vibeke did something wrong, but Herdr has not run the same step yet. Or Herdr passed, but the harness could not compare the dependency bytes of the two sides. |
| `compat_failure` | Herdr passes the step and Vibeke fails it, or the result breaks the pinned schema. It links to an inventory row. |
| `dependency_failure` | A build or an external dependency failed or changed. |
| `harness_error` | The harness, runner or scenario is broken. |
| `not_exercised` | The step did not run. It always has a reason. |

One rule decides between Vibeke, dependency and baseline failures for a step:

1. If the two sides did not receive the same dependency bytes, the outcome is `dependency_failure`. In `verified` mode the lock guarantees equal bytes. In `unpinned` mode the harness runs both sides in the same boundary, one after the other, and compares the hashes of what each downloaded.
2. If Herdr has not run the step, a Vibeke failure is `suspected_compat_failure`.
3. If Herdr passes and Vibeke fails, the outcome is `compat_failure`. This needs proof that both sides received the same dependency bytes. In `unpinned` mode the harness may not see the downloads. Then it cannot compare the bytes, and the outcome is `suspected_compat_failure` with the reason "dependency bytes not compared (unpinned)".
4. If both sides fail, the outcome is `not_exercised`, with the Herdr result as the reason.

A plugin's outcome is its worst step outcome. A step that has a known result and now passes is an **unexpected pass**, and fails the run until someone updates the scenario. This keeps known failures from hiding fixes and new failures.

### 8.5 First plugins

The first plugins must cover the five categories in 07 §8.4 item 2. Each category needs a confirmed plugin before the harness claims it:

| Category | Candidate | How it is confirmed |
|---|---|---|
| Layout or worktree workflow | `cloudmanic/herdr-plus` (handles `worktree.created` and `worktree.opened`) | Its scenario creates a worktree and checks the resulting layout. |
| Event notification | `ZingerLittleBee/Heeler` (posts to a push relay URL from its config) | Its scenario changes agent status and checks the stubbed HTTP endpoint. `dcolinmorgan/herdr-remote` sends a UDP datagram to a fixed local port, so its scenario checks only that its hooks finish. |
| Link handler | `ChmaraX/herdr-nvim` | Its scenario opens a matching URL. |
| Stateful startup restoration | `eliasstravik/herdr-projects` | Its scenario restarts the server and checks restored state. |
| Raw socket callbacks | `alexarthurs/herdr-sidebar` (JSON lines to `HERDR_SOCKET_PATH`) | Source review found direct socket use. `scripts/herdr-plugins-scan.ts` confirms it on the mirrored source in CI. `cloudmanic/herdr-plus` and `persiyanov/herdr-reviewr` also use the socket. |

## 9. Observation

The harness first reuses what exists: `events.read`, invocation logs, the `plugin.api_call` audit and pane-open audits.

It adds narrow, test-only tracing (`VIBEKE_HERDR_TRACE=<file>`) only for what those miss:

- Launcher arguments and CLI parse errors. These happen in `vk-cli` before the broker sees the call.
- Every broker result, read-only calls included, for CLI clients and raw socket clients alike.

Each trace line has the plugin id, invocation id, transport, method, result and error code. Params are left out unless a scenario asks for them. Then they are redacted. Scenarios compare this trace with their expected calls (§8).

## 10. Visual comparison

### 10.1 Capture prototype first

Running Herdr without a window is plausible but not yet proven. A pseudo-terminal (PTY) alone is not enough. The driver must also answer terminal queries, send input, resize, detect readiness and shut down cleanly. `vk-term` can decode Herdr's output. It also emits `Effect::Reply` for terminal queries (`crates/vk-term/src/engine.rs`), and the driver must write those replies back to the PTY. Milestone 2 builds this for one plugin, on both sides, before any wider rollout.

### 10.2 Geometry

Placement and contents are checked separately.

- **Placement.** The harness records the position and size of every plugin pane after each step. On Herdr, `pane.layout` gives the rectangles at the pinned commit. These rectangles include the pane frame. A placement that differs from Herdr's (for example, a split on the other side) is a finding.
- **Contents.** The harness compares contents only when both plugin processes see the same inner PTY size, in columns and rows. Equal outer terminal sizes are not enough, because the two programs draw different frames. If the inner sizes differ, the scenario first resizes the pane on each side to a size it declares. If that is impossible, the step compares placement only.
- **Cropping.** Herdr's crop removes the frame from the `pane.layout` rectangle using the frame width at the pinned commit. Zoomed panes, overlays and popups each have their own crop rule, which the prototype confirms. Vibeke needs no crop. It captures the pane's own grid.

### 10.3 Canonical cell grid

Both sides are converted to one structured format before comparison. Neither side uses the ANSI text form, because it trims blanks and merges underline styles.

- **Size.** Exactly the inner PTY size. Blank cells are kept.
- **Text.** One grapheme per cell. A wide character takes its cell plus a marked continuation cell. Combining marks stay with their base character.
- **Colors.** Default, indexed (0–255) or RGB. Both sides start with the same fixed palette and theme. A plugin can change palette entries (OSC 4) and the default colors (OSC 10 and 11). The two engines record such cells differently: Herdr's capture turns a changed palette entry into RGB, and `vk-term` keeps the index. So the harness tracks palette changes on each side. A cell whose index points at a changed entry becomes RGB on both sides. Unchanged indexed colors stay indexed and are compared as indexes.
- **Attributes.** First version: bold, italic, inverse and "any underline". Other attributes come later.
- **Cursor.** Position and visibility are recorded, but not compared by default.

Vibeke needs a structured grid export for this. It will be either a test-only API or a new `pane.screenshot` format. The choice follows the `vibeke/1` additive rules.

### 10.4 Stability and masks

Each scenario can declare masks for regions that change on their own, such as clocks, spinners, temp paths and ids. A mask is a rectangle or a regular expression. It is never "ignore the pane". The harness applies masks **before** it checks stability. Otherwise a spinner would keep the screen from ever being stable. A screen is stable when its masked grid has not changed for a set time, within a timeout.

A step passes when all unmasked cells match. For plugins that draw live external data, such as browsers, dashboards or agent status, the scenario compares placement and fixed text only. Or it marks the step `not_exercised` with a reason.

### 10.5 Report

Both canonical grids go through the same renderer, so fonts cannot create false differences. For each plugin and step, the report shows Herdr, Vibeke and a diff overlay. The diff is computed from the grids, not from the images. The report is uploaded as a workflow artifact.

Before Herdr runs, Vibeke's canonical grids can be checked in as reviewed reference snapshots. A change in how a plugin renders then shows up in review.

## 11. Differential mode against Herdr 0.9.3

- Build Herdr from source at `7b116c05bfda646af39d2524c54e70c751f57ee8`, or fetch the release asset. Record its sha256 in `baseline.toml`. Verify the sha256 **before** running the binary. This also fixes the order in `compat_herdr_diff.rs`. The harness never uses an installed Herdr.
- Capture `herdr api schema --json` and every CLI help page into the `pending` entries of `baseline.toml`.
- Run each scenario against Herdr in its own temp HOME, XDG directories and runtime directory. Vibeke's trace cannot see inside Herdr, so Herdr's calls are recorded by a proxy on its socket.
- The two sides run one after the other. First every step runs on Vibeke. Then the harness stops everything the Vibeke side started: the TUI client, the server, the pane holders, and every process whose working directory or environment points into the Vibeke side's directories, also daemons in their own sessions. It waits until those processes are gone and the TCP ports they listened on are free. Then every step runs on Herdr. Last, the two records are compared step by step. So a daemon of one side never answers the other side, and each side keeps its own state from step to step.
- Herdr runs even when the Vibeke install or build fails. Herdr installs and builds the plugin itself.
- Both sides get the same terminal size and the same sidebar width. The Vibeke side gets a config file without the setup guide and without update checks.
- Both sides run every step in the same way:
  - **Actions:** the socket method `plugin.action.invoke`, with the same context in Herdr's field names (`workspace_id`, `tab_id`, `focused_pane_id`).
  - **Input:** `pane.send_input` for a plugin pane. A popup has no pane id, so its keys go through the attached TUI client.
  - **Links:** on Herdr, the harness prints the URL in the context pane and clicks it with `pane.link.activate`.
  - **Restart:** on Herdr, `server stop`, then the server and the TUI start again with the same directories.
- Herdr's evidence:
  - **Plugin panes** are ordinary panes in `pane.list`, labelled with the manifest's pane title. A step's pane is a new pane with that label. `pane.layout` gives its placement and rectangle.
  - **Popups** have no pane id. The harness finds a popup on the screen where Herdr's popup rules (`src/popup_size.rs`) place it, with the manifest's pane title in its frame. It captures a popup only when the step's trace shows that the popup opened, or when the popup stays open from an earlier step. Popups, overlays, zoomed panes and tab panes keep their size, and both sides compare them at that size.
  - **Events:** at start the harness subscribes to every Herdr event kind with `events.subscribe`, and to the fake agent pane's status. It logs every event line, and `event` expectations read that log.
- On the Vibeke side, the compatibility layer answers `pane.layout` in Herdr's shape, so both sides measure placement the same way.
- When a step fails on either side, the harness keeps the grid of every pane the step names and the client screen, so the report shows what was on screen.
- Compare traces, logs, pane placement, files in the plugin state directory and canonical grids (§10).
- Use a stricter normalizer than the current one, which replaces every `*_id`, every numeric `*_ms` and every error message. Each scenario lists the fields to normalize. Stable plugin and action ids are compared exactly.
- Outcomes follow the single rule in §8.4.

## 12. Runner and reports

- A new integration test file, `crates/vibeke/tests/it/compat_herdr_real_plugins.rs`, gets a `mod` line in `tests/it/main.rs`. It has one `#[ignore]` test per locked plugin. CI runs them with `--run-ignored only` and `VIBEKE_HERDR_PLUGINS=1`. Without the variable, each test fails with a clear message and runs no plugin code.
- `tests/it/support/mod.rs::Session` gains HOME and environment control. The plugin helpers that are private to `compat_herdr_plugins_ext.rs` move into shared support. They are moved, not copied.
- Each test writes `target/herdr-plugins/<plugin>-<os>.json`. The reporting job (§5) builds one JSON and Markdown matrix from them: plugin, rank, platform, outcome per step, Herdr surfaces used, and failed or partial surfaces.
- Runtime results stay out of the generated inventory. `render_markdown` gains a fixed link to the latest matrix, so the inventory equality test keeps passing.
- A new workflow, `herdr-plugins.yml`, runs weekly and on manual dispatch. It has a trusted build job, one plugin job per boundary on Linux and macOS, and the reporting job.

## 13. Coverage on pull requests

There is no general trace replayer. Replay would need initial state, id bindings taken from responses, plugin authority, event order and streams. It would also only prove that Vibeke matches itself.

Instead, calls seen in real runs become small reviewed fixtures in `tests/compat/herdr/0.9.3/plugins/fixtures/`. Each fixture has an initial state, a call and an expected response shape. Normal CI runs them against the compat endpoint, through a real broker with the plugin's authority, so broker restrictions stay in the path. These fixtures catch regressions. They do not prove that Herdr behaves the same way.

## 14. Milestones

1. **Isolation and one plugin (Linux).** The workflow with the §5 boundary, the lock and fetch scripts, the `Session` changes, the coverage checker and one scenario (`eliasstravik/herdr-projects`). Output is the JSON report artifact only. No issue automation, no shared build cache, `unpinned` dependency mode.
2. **Capture prototype.** Herdr 0.9.3 built and checksummed. The PTY driver (§10.1), the structured Vibeke grid export, the canonical grid and the crop rules for the same plugin on both sides. Baseline schema and CLI help captured.
3. **Five plugins.** The five categories in §8.5, with the raw socket plugin confirmed by the source scan. Narrow tracing (§9), `verified` dependency mode, Herdr comparison for each step.
4. **All 20 plugins.** macOS runner, the aggregate matrix, the reporting job with issue updates, the drift check and the dependency cache.
5. **Pull request fixtures** (§13) and reference snapshots.

## 15. Risks and open questions

- The capture prototype may show that Herdr's frames or popups cannot be cropped reliably. Then the visual comparison falls back to placement and text checks for those placements.
- Some plugins need external services or agent CLIs. Stubbing them without changing plugin code may not work for every step. Those steps end as `not_exercised`.
- The `verified` proxy may break plugins that pin certificates or use protocols other than HTTP. Those plugins stay `unpinned`, and their reports say so.
- Twenty builds, some with cargo or go, may take 20 to 40 minutes without a cache. Separate jobs run in parallel, and the dependency cache makes later runs shorter.
- Popularity changes every week. The lock and the drift report keep the corpus stable between reviewed updates.
