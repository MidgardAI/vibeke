# OpenCode

Install the plugin to see OpenCode state in the sidebar and to answer its permission requests from the inbox.

## What you get

| Feature | How it works |
|---|---|
| State | The plugin reports turns, tool calls, retries, errors and compaction. A turn stays working while a subagent session still runs. |
| Approvals | A permission request from OpenCode becomes an inbox card. When you answer the card, the plugin sends your answer to OpenCode. OpenCode closes its own dialog. |
| Answer in the pane | OpenCode keeps showing its dialog while the card is open. If you answer in the pane, Vibeke closes the card. |
| Allow always | **Allow always** on a card uses the "Allow always" choice of OpenCode. OpenCode then allows the same pattern for the rest of the session. |
| File changes | The plugin reports the files that OpenCode edits. |
| Usage | The plugin reports tokens and cost for each answer. |
| Resume | Vibeke can resume a session with `opencode --session <id>`. |

## Install

1. Open **Setup** in the command palette. Select OpenCode. Confirm.
2. Or run the commands below. Without `--yes`, the command only prints the planned change.

```sh
vibeke integration install opencode
vibeke integration install opencode --yes
```

3. Restart OpenCode.

## Files it changes

The installer writes the plugin to `~/.config/opencode/plugins/vibeke.ts`. If `XDG_CONFIG_HOME` is set, the folder is `$XDG_CONFIG_HOME/opencode/plugins/`.

It also writes the Vibeke skill to `~/.agents/skills/vibeke/SKILL.md`. Codex, pi, oh-my-pi and Gemini CLI read the same file.

The first line of the file marks it as managed by Vibeke. A reinstall overwrites it. If a file exists at that path without the mark, the installer refuses to replace it. The installer also removes a managed copy in the older `plugin/` folder, so that OpenCode does not load the plugin twice.

The plugin stays inactive outside a Vibeke pane.

## Check

```sh
vibeke integration status opencode
vibeke integration doctor opencode
```

`doctor` compares the OpenCode version with the validated range.

## Remove

```sh
vibeke integration uninstall opencode --yes
```

This deletes the managed plugin file. It refuses to delete a file that Vibeke did not write.

## Limits

- Vibeke checked the plugin against OpenCode 1.19. With another version, Vibeke still shows the state, but you answer permission requests in the pane.
- Without the plugin, Vibeke reads the screen. It shows a permission request, but it cannot answer it with keystrokes. OpenCode marks the selected choice only with a color. Answer in the pane then.
- `opencode serve`, `opencode web` and `opencode acp` do not report through the plugin. These commands serve sessions for other programs.
- Questions from the OpenCode question tool do not appear in the inbox. Answer them in the pane.

## Next steps

- [Agents and interactions](../concepts/agents.md): requests, answers and background work.
- [Customize Vibeke](../customize.md): change settings and keys.
- [Troubleshooting](../troubleshooting.md): fix a plugin that does not report.
