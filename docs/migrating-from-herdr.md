# Import from Herdr

Vibeke uses separate configuration, sockets, and state. The importer reads Herdr files. It does not modify Herdr configuration, hooks, or active sessions.

## Install Vibeke

Follow the [installation guide](site/src/install.md) to install the published CLI.
First-run setup offers to import an existing Herdr configuration. Use the commands below for a detailed migration.

The installer does not use `sudo`. It creates `~/.local/bin/vibeke`. Add that directory to `PATH` if necessary.

Check the installation:

```sh
vibeke doctor
```

Use `vibeke doctor --json` for JSON output. Use `--no-remote` to omit remote checks.

## Preview the import

```sh
vibeke import herdr --dry-run
```

Read the import report before the next step.

## Import configuration and layout

```sh
vibeke import herdr
```

This imports configuration and recreates workspaces from `~/.config/herdr/session.json`. Use `VIBEKE_SESSION=herdr-import` to select a named Vibeke session.

For configuration only, use:

```sh
vibeke import herdr --config
```

The importer's `--session` option conflicts with the global `--session NAME` option. It can consume the next argument, including `--dry-run`. Do not use the importer's `--session` option. Use plain import or `--dry-run` instead.

### Supported settings

- Theme names, automatic theme selection, and custom theme values.
- Terminal shell, shell mode, and working directory settings.
- Setup and update settings.
- Prefix and action keys. Legacy indexed keys become tab, workspace, and agent key ranges.
- Custom commands, sidebar settings, token rules, and `remote_image_paste`.
- The worktree directory, which becomes `tasks.root`.
- Workspace, tab, pane layout, and working directory records.
- Stored Claude/Codex session IDs as resume candidates.

Bindings with `cmd` or `super` require the Kitty keyboard protocol. The importer reports a warning for those bindings.

The importer prints agent resume commands. It does not start agents. Active processes remain in Herdr.

Unsupported settings appear as `skipped` in the report. The importer does not activate plugins.

If Vibeke configuration exists, the importer writes `config.imported.toml` beside it. Use `--force` only to replace existing configuration.

## Herdr plugins

Compatibility is partial. See the [compatibility inventory](herdr-compat-inventory.md).

1. Register a local plugin:

   ```sh
   vibeke plugin link ~/src/my-herdr-plugin
   ```

2. Review and trust its commands:

   ```sh
   vibeke plugin trust <id> --legacy
   ```

No plugin command runs before trust. `vibeke plugin install <dir>` also registers a local directory.

### Copy plugin configuration and state

1. Preview the copy:

   ```sh
   vibeke plugin migrate --from ~/herdr-backup --dry-run
   ```

2. Copy the files and register the plugins:

   ```sh
   vibeke plugin migrate --from ~/herdr-backup --link
   ```

3. To remove files created by the last migration, use:

   ```sh
   vibeke plugin migrate --rollback
   ```

The source can be `~/.config/herdr` or a backup. Migration copies files. It does not move them. Existing destination files remain unchanged and appear as conflicts.

Herdr plugin directory layouts are not verified. The migration checks these paths:

- `plugins/<id>/config|state`.
- `plugins/config|state/<id>`.
- `plugin-config|state/<id>`.
- Explicit `config_dir` and `state_dir` entries in `plugins.json`.

The report identifies the matching layout. Plugins registered with `--link` still need explicit trust.

### Supported plugin features

- Actions, event hooks, and startup hooks through a private `herdr` launcher and socket.
- Worktree, layout, pane move/swap, agent, and metadata calls.
- Plugin panes in all placements.
- Plugin actions in the command palette (`prefix+:`). Untrusted or disabled actions remain unavailable.
- Configuration bindings with `[[keys.command]] type = "plugin_action"`.
- URL handlers through `prefix+shift+u` or Ctrl/Alt+click.
- Window titles through `ui.title_sync`, with tab-bar titles as the alternative.
- `herdr --session NAME`.

A popup covers part of the layout using its declared width and height. An overlay covers the current tab.

A plugin pane closes when its command exits. Use `prefix+x` to close it manually. For popups, `herdr popup close` also works.

Startup-hook background processes retain callback access across server restarts. Background processes from an action lose access when that action ends. Plugin logs redact recognized credentials.

Unsupported features are `agent.view.set/clear`, key bindings from plugin manifests, and Git installation. Put key bindings in your configuration instead.

### Compatibility socket

Set `[compat.herdr] enabled = true` to expose a Herdr-compatible socket for existing socket clients.

The default session uses `$RUNTIME/herdr-compat/herdr.sock`. Named sessions use `$RUNTIME/herdr-compat/sessions/<name>/herdr.sock`.

These paths are inside Vibeke's runtime directory. They do not use `~/.config/herdr`.

## Terminal controls

Vibeke uses the same `ctrl+b` prefix by default.

| Action | Key |
| --- | --- |
| Open the inbox | `prefix+i` |
| Open copy mode | `prefix+[` |
| Search in copy mode | `/` |
| Edit scrollback | `prefix+e` |
| List active bindings | `vibeke keys` |

Requests from other panes appear in the inbox. The focused agent can ask in its own pane. Holder processes preserve panes after a server restart. Multiple clients can connect with separate focus and viewport state.

Use `vibeke --default-config` to print the full configuration template.

## Use both applications

Vibeke uses its runtime directory and `~/.local/state/vibeke`. Herdr uses its own sockets and configuration. They do not share state.

Vibeke removes `HERDR_*` environment variables inside its panes. This prevents inherited hooks from contacting the wrong server. Start Vibeke in a separate terminal tab instead of inside Herdr.

Vibeke preserves Herdr hook files and hook entries. Its integrations install beside them.

### Configure agent integrations

1. Check integration status:

   ```sh
   vibeke integration status all
   ```

2. Preview the changes:

   ```sh
   vibeke integration install claude
   ```

3. Apply the changes:

   ```sh
   vibeke integration install claude --yes
   vibeke integration install codex --yes
   ```

Without `--yes`, the installer shows changes without applying them. `--dry-run` also previews changes.

To use configuration copies, set `CLAUDE_CONFIG_DIR` or `CODEX_HOME` to those copies.

For Codex, open `/hooks` after installation. Review the Vibeke entries before you trust them. `vibeke doctor` reports untrusted hooks.

Shell aliases or functions named `codex` or `claude` can bypass the command wrapper in `PATH`. `vibeke doctor` checks for this condition.

## Remote hosts

1. Build the remote release files with `mise run dist`.
2. Read the [release verification requirements](releases.md).
3. Add the host:

   ```sh
   vibeke machine add devbox me@devbox.example.com
   ```

4. Connect:

   ```sh
   vibeke ssh devbox
   ```

Vibeke installs under `~/.local` on the remote host without `sudo`. The remote host verifies the uploaded checksum.

After a local upgrade, use `vibeke ssh devbox --upgrade`. Holder processes preserve remote panes during the server upgrade.

`vibeke --machine devbox <noun> <verb>` sends a command to that host. It does not substitute the local host if the connection fails.

Plain SSH supports terminal use. File-drop translation and clipboard image paste require the local Vibeke client. Use `vibeke ssh <host>` from your local computer for those features.

## Terminal settings

### Ghostty

Ghostty supports the Kitty keyboard protocol and synchronized updates by default.

For clipboard copy, set `clipboard-write = allow` in Ghostty configuration. The default `ask` value requests permission for each copy.

### iTerm2

1. Open **Settings → General → Selection**.
2. Enable **Applications in terminal may access clipboard** for OSC 52 copy.
3. Open **Profiles → Keys → General**.
4. Enable **Report keys using CSI u** for modified keys.

Truecolor is enabled by default. With plain SSH, check that `COLORTERM=truecolor` reaches the remote host.

`vibeke doctor` checks terminal features and prints the applicable settings.

## Restore or remove Vibeke

| Action | Command or path |
| --- | --- |
| Restore the previous version | `vibeke update --rollback`. Switches `current` and restarts the server. Holders preserve panes. |
| Remove Claude hooks | `vibeke integration uninstall claude --yes`. Removes only Vibeke entries. |
| Remove Codex hooks | `vibeke integration uninstall codex --yes`. Removes only Vibeke entries. |
| Stop only the server | `vibeke server stop`. Holder processes remain active. |
| Stop the server and panes | `vibeke api call server.stop '{"kill_panes":true}'` |

To remove Vibeke completely, delete `~/.local/bin/vibeke`, `~/.local/share/vibeke`, `~/.local/state/vibeke`, and `~/.config/vibeke`.

Herdr configuration and sessions remain unchanged.
