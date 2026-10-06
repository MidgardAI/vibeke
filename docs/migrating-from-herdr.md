# Migrating from Herdr to Vibeke

Vibeke runs next to Herdr. Nothing here touches `~/.config/herdr`, Herdr's sockets or sessions,
or Herdr's hooks, so you can try Vibeke for as long as you like and keep Herdr as the fallback.

## 1. Install

From a release:

```sh
curl -fsSL https://github.com/MidgardAI/vibeke/releases/download/v0.1.0/install.sh | sh
```

Or from a local build (`mise run dist`, then):

```sh
VIBEKE_INSTALL_FROM=dist/0.1.0 sh scripts/install.sh
```

The installer verifies the checksum, puts the binary in `~/.local/share/vibeke/versions/<v>/`,
points `~/.local/share/vibeke/current` at it, and links `~/.local/bin/vibeke`. It never uses sudo.
Make sure `~/.local/bin` is on your `PATH`, then check the setup:

```sh
vibeke doctor
```

`doctor` reports install, sockets, integrations, terminal capabilities, remote machines and
topology, with a fix line for everything that is not green. `vibeke doctor --json` is the
machine-readable form, and `--no-remote` skips the SSH probes.

## 2. Import your Herdr config and sessions

Look first, change nothing:

```sh
vibeke import herdr --dry-run
```

Then import:

```sh
vibeke import herdr               # config and session
vibeke import herdr --config      # only ~/.config/herdr/config.toml -> ~/.config/vibeke/config.toml
```

Plain `vibeke import herdr` imports the config and recreates the workspaces from
`~/.config/herdr/session.json` in a Vibeke session. To put them in a named session instead of
`default`, set `VIBEKE_SESSION=herdr-import` for the command. The importer only reads Herdr's
files.

Known issue: the importer's `--session` flag (session only) collides with the global
`--session NAME` flag, which swallows the next argument. `vibeke import herdr --session --dry-run`
silently drops the `--dry-run`. Do not use it until this is fixed; use `--dry-run` alone or no
flag, as above.

### Mapped

- Theme (`name`, `auto_switch`, `dark_name`, `light_name`, `[theme.custom]`), `[terminal]`
  (`default_shell`, `shell_mode`, `new_cwd`), `onboarding`, `[update]`.
- `[keys]`: the prefix and every action key under the same action name. Legacy `[keys.indexed]`
  becomes the `switch_tab` / `switch_workspace` / `focus_agent` ranges. Bindings using `cmd` or
  `super` get a warning, because they need a terminal with the kitty keyboard protocol.
- `[[keys.command]]` custom commands, `[ui]` sidebar settings and sidebar token rules,
  `remote_image_paste`.
- `[worktrees] directory` becomes `tasks.root`.
- `session.json`: workspaces, tabs, pane layout and working directories are recreated.
- Agent sessions: the stored agent session ids (Claude/Codex) become resume candidates. Vibeke
  prints the resume command for each; it does not start agents on its own.

### Not mapped

- Anything the importer cannot place is listed as `skipped` in its report. Read the report.
- Herdr plugins are inventoried, not activated. Plugin support arrives later.
- Running agent processes in Herdr stay in Herdr. Vibeke recreates the layout and offers resume,
  it does not take over live processes.

If `~/.config/vibeke/config.toml` already exists the config import writes
`config.imported.toml` next to it instead of overwriting; use `--force` to overwrite.

## 3. Key differences

- **Prefix** is `ctrl+b` by default, and the action names and default bindings are the
  same. Imported `[keys]` keep your own prefix.
- **`prefix+a`** jumps to the next pane that needs attention (an agent waiting for you).
- **`prefix+i`** opens the inbox: questions, approvals and notifications from all agents.
- **Interaction cards** (an agent asking a question or requesting an approval) pop up only for
  agents in panes you are *not* looking at; the focused agent asks in its own pane. Change this
  with `ui.interaction_overlay` (`off | unfocused | always`).
- **`prefix+[`** enters copy mode with vi keys; press `/` inside it to search the buffer.
  `prefix+/` searches scrollback directly, `prefix+e` opens scrollback in your editor.
- **Server and panes survive.** Each pane is held by a small holder process, so a server crash,
  restart or upgrade keeps every shell and agent running.
- **Multiple clients** can attach to one session with independent focus and viewport.

`vibeke keys` lists the active bindings and `vibeke --default-config` prints the full default
config with comments.

## 4. Running both side by side

- Separate sockets and state: Vibeke uses its own runtime dir and `~/.local/state/vibeke`;
  Herdr keeps `~/.config/herdr` and its own sockets. There is no shared state.
- Inside Vibeke panes, `HERDR_*` environment variables are stripped, so Herdr's hook scripts and
  CLI do not talk to a Herdr server that is not the one hosting the pane. Do not start Vibeke
  from inside a Herdr pane expecting nesting to be seamless; use a separate terminal tab.
- Herdr's hooks (`~/.claude/hooks/herdr-*`, Herdr entries in `~/.codex/hooks.json`) are left
  untouched. Vibeke's integration installs next to them.

### Agent integrations (consent first)

```sh
vibeke integration status all
vibeke integration install claude          # shows the diff, writes nothing
vibeke integration install claude --yes    # writes ~/.claude/settings.json hooks
vibeke integration install codex --yes
```

Without `--yes` the command only shows what it would change. `--dry-run` is also available. To try
it without touching your real configs, point `CLAUDE_CONFIG_DIR` / `CODEX_HOME` at a copy.

Codex asks you to trust new hooks: start Codex once and run `/hooks`, review the Vibeke entries
and trust them. `vibeke doctor` warns while any Codex hook is untrusted.

If you have a shell alias or function named `codex` (or `claude`), it can bypass the PATH shim
Vibeke uses inside panes. `vibeke doctor` checks for this.

## 5. Remote machines

Recommended: run the client on your laptop and let it manage the remote.

```sh
vibeke machine add devbox me@devbox.example.com
vibeke ssh devbox
```

`vibeke ssh` probes the host, installs or upgrades Vibeke under `~/.local` on the remote (no sudo,
checksum re-verified on the remote), then attaches over an SSH-multiplexed link. Build the Linux
artifacts first with `mise run dist` if you have not. After a Vibeke upgrade locally, run
`vibeke ssh devbox --upgrade`; panes on the remote survive the upgrade.

`vibeke --machine devbox <noun> <verb>` forwards any CLI command to that machine and never falls
back to local.

Plain `ssh devbox` and then `vibeke` works for terminal use, but **drop and paste translation and
clipboard image paste need the local client**. Dropping a screenshot into a remote agent pane, or
pasting an image, only works through `vibeke ssh <host>` from the laptop. `vibeke doctor` says so
when it detects it is running over plain ssh.

## 6. Terminal settings

### Ghostty

- Kitty keyboard protocol and synchronized updates work out of the box.
- For clipboard copy from Vibeke (and from remote Neovim) set `clipboard-write = allow`
  in the Ghostty config. The default `ask` prompts on every copy.

### iTerm2

- Settings > General > Selection > enable **Applications in terminal may access clipboard**
  (OSC 52 copy).
- Profiles > Keys > General > enable **Report keys using CSI u** so modified keys such as
  `ctrl+shift+…` reach Vibeke.
- Truecolor is on by default; check that `COLORTERM=truecolor` survives your SSH hops if you use
  plain `ssh`.

`vibeke doctor` runs a live capability probe in your terminal (kitty keyboard, synchronized
updates, truecolor, background) and prints these hints for the host it detects.

## 7. Rolling back

Vibeke never changed Herdr, so rolling back is mostly "keep using Herdr".

- **Bad Vibeke upgrade:** `vibeke update --rollback` switches `current` to the previous version
  and restarts the server; panes survive.
- **Remove the hooks:** `vibeke integration uninstall claude --yes` and `... codex --yes` remove
  only Vibeke's entries and leave Herdr's alone.
- **Stop Vibeke:** `vibeke server stop` (this leaves the shells running in their
  holders; `vibeke api call server.stop '{"kill_panes":true}'` stops them too).
- **Remove it completely:** delete `~/.local/bin/vibeke`, `~/.local/share/vibeke`,
  `~/.local/state/vibeke` and `~/.config/vibeke`. Herdr's files are not involved.
- Your Herdr config and session are untouched, so `herdr` starts exactly where you left it.
