# OpenCode

Install the OpenCode plugin to see OpenCode state in the sidebar and to answer its permission requests.

The OpenCode integration has not been checked against a live OpenCode release. Vibeke names no validated version. Every version runs with the basic capabilities until a version is validated. Read the limits below before you rely on it.

## What you get

| Feature | How it works |
|---|---|
| State | The plugin reports when a session starts, a message is sent, a tool runs, the session is idle, an error occurs and the session is compacted. |
| Subagents | A session with child sessions stays in the working state until every child is idle. |
| File changes | The plugin reports files that OpenCode edits. |
| Permission requests | A permission request becomes an inbox card. |
| Usage | The plugin reports tokens and cost from completed assistant messages. |

By default, Vibeke answers an OpenCode permission request by sending keystrokes to the pane. The plugin can also answer with allow or deny. The manifest lists this as documented but not verified, so `capabilities` shows it as unverified.

## Install

1. Open **Setup** in the command palette. Select OpenCode. Confirm.
2. Or run the commands below. Without `--yes`, the command only prints the planned change.

```sh
vibeke integration install opencode
vibeke integration install opencode --yes
```

3. Restart `opencode` in a Vibeke pane.

## Files it changes

- `~/.config/opencode/plugin/vibeke.ts`. If `XDG_CONFIG_HOME` is set, the file is in `$XDG_CONFIG_HOME/opencode/plugin/`.
- The first line marks the file as managed by Vibeke. A reinstall overwrites it. The installer refuses to replace a file without the mark.

The plugin runs `vibeke hook opencode <event>` for each event it forwards. It stays inactive outside a Vibeke pane.

## Check

```sh
vibeke integration status opencode
vibeke integration doctor opencode
vibeke integration capabilities opencode
```

`doctor` prints the OpenCode version. It shows `observe` and `answer_keystroke` as the active capabilities. It lists the other documented capabilities as unverified.

## Remove

```sh
vibeke integration uninstall opencode --yes
```

This deletes the managed plugin file.

## Limits

- Event names and payloads follow the OpenCode documentation. Vibeke has not exercised them against a live release. Your version may differ.
- Vibeke writes to the `plugin/` folder. Some OpenCode releases may read `plugins/` instead. If OpenCode does not load the plugin, check which folder your version reads.
- Vibeke does not read an OpenCode transcript file.
- Vibeke has no structured model switch for OpenCode.

## Next steps

- [Other agents](other.md): run OpenCode as an ACP agent, or add your own agent.
- [Agents and interactions](../concepts/agents.md): requests, answers and background work.
- [Troubleshooting](../troubleshooting.md): fix a plugin that does not report.
