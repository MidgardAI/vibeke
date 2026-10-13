# Hermes Agent

Install the plugin to see Hermes Agent state in the sidebar and to answer its approval prompts from the inbox.

## What you get

| Feature | How it works |
|---|---|
| State | The plugin reports turns, tool calls, errors and the end of a session. Turns of subagents do not end the turn of the main session. |
| Approvals | A dangerous-command prompt becomes an inbox card. When you answer the card, Vibeke selects the choice in the Hermes dialog with the arrow keys. |
| Allow always | **Allow always** on a card selects "Allow for this session" in Hermes. |
| Questions | A clarify question or a sudo password prompt becomes an inbox card. Answer it in the pane. Vibeke closes the card when Hermes has the answer. |
| Usage | The plugin reports tokens for each model call. Hermes does not report a cost. |
| Messages | When another pane sends a message, the plugin hands it to Hermes at the start of the next turn. If Hermes is idle, Vibeke types the message into the prompt. |
| Resume | Vibeke can resume a session with `hermes --resume <id>`. |

The plugin only observes. Hermes does not let a plugin answer an approval. This is why Vibeke answers approvals with keystrokes.

## Install

1. Open **Setup** in the command palette. Select Hermes Agent. Confirm.
2. Or run the commands below. Without `--yes`, the command only prints the planned change.

```sh
vibeke integration install hermes
vibeke integration install hermes --yes
```

3. Restart Hermes.

## Files it changes

| File | Change |
|---|---|
| `~/.hermes/plugins/vibeke/plugin.yaml` | New file. |
| `~/.hermes/plugins/vibeke/__init__.py` | New file. |
| `~/.hermes/config.yaml` | Adds `vibeke` to `plugins.enabled`. |
| `~/.hermes/skills/vibeke/SKILL.md` | New file: the Vibeke skill. |
| `~/.hermes/skills/vibeke-remote/SKILL.md` | New file: the skill for the messaging gateway. |

If `HERMES_HOME` is set, Vibeke uses that folder instead of `~/.hermes`.

The installer saves a backup of `config.yaml` before its first change. It changes only the `plugins.enabled` list and keeps your comments and other settings. If `vibeke` is in `plugins.disabled`, the installer removes it there.

The skill teaches Hermes the `vibeke` commands for panes, agents, tasks, previews, the agent browser and images. A line after the header of the skill marks it as managed by Vibeke. The installer does not replace a `vibeke` skill without that line.

The `vibeke-remote` skill follows the same rules as the `vibeke` skill. The installer does not replace a file without the mark. `integration status` lists it, and a missing one makes the status partial.

The first line of each plugin file marks it as managed by Vibeke. A reinstall overwrites it. If a file exists at that path without the mark, the installer refuses to replace it.

The plugin stays inactive outside a Vibeke pane.

## Start work from Slack or another chat

Hermes can run a messaging gateway, for example for Slack. The `vibeke` skill stops outside a Vibeke pane. The `vibeke-remote` skill is for this case. It lets Hermes start an agent on the host and post links for your team.

Hermes runs on the host, outside every pane. It uses the `vibeke` command with your user account. It needs no extra login. Your host needs the gateway (`vibeke gateway on`) and an account login (`vibeke login`) for links.

A typical chat:

1. A teammate writes: "@hermes add a dark mode toggle to the settings page".
2. Hermes creates a workspace, starts an agent and sends it the task.
3. Hermes posts a **watch link** so the team can follow the agent.
4. When the agent declares its preview, Hermes posts a **review link**. Designers try the app and send notes.
5. After the agent opens a pull request, Hermes posts a **continue link** and comments on the pull request.

The links are limited to GitHub accounts and work for several people. Each teammate signs in with GitHub once. See [work with a teammate](../sharing.md) and [move work to another host](../handoff.md#continue-a-session-from-a-link).

The skill tells Hermes to follow these rules:

- Post links, never terminal output.
- Ask the requester before it uses `--any-github` or `--link`, which let more people in.
- Tell the requester which GitHub accounts and teams can open each link.

The links are for the people and teams on your sharing list. Before Hermes posts links, add your teammates on your [account page](../sharing.md#your-sharing-list). You can also create a [team](../sharing.md#teams) there and invite them. Everyone in the team can then open each link that Hermes makes for the team.

## Check

```sh
vibeke integration status hermes
vibeke integration doctor hermes
```

`doctor` compares the Hermes version with the validated range.

## Remove

```sh
vibeke integration uninstall hermes --yes
```

This deletes the plugin folder and both skills, and removes `vibeke` from `plugins.enabled`. It refuses to delete a file that Vibeke did not write.

## Limits

- Vibeke checked the plugin against Hermes Agent 0.21.
- Vibeke answers approvals in the classic interface of Hermes. In the TUI (`hermes --tui`), answer approvals in the pane.
- Hermes has no hook for the end of a turn. A message that arrives during a turn waits for the next turn, or for the prompt to become idle.
- With `approvals.mode: smart`, a model decides most approvals without a prompt. Vibeke then shows no card for them.
- The plugin reports only sessions that run in the terminal. It ignores the messaging gateway and ACP sessions.

## Next steps

- [Agents and interactions](../concepts/agents.md): requests, answers and background work.
- [Customize Vibeke](../customize.md): change settings and keys.
- [Troubleshooting](../troubleshooting.md): fix a plugin that does not report.
