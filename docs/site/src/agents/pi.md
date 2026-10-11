# pi and oh-my-pi

Install the extension to see pi or oh-my-pi (omp) state in the sidebar and to answer extension dialogs from the inbox.

The two agents share one extension. You install it for each agent with its own ID: `pi` or `omp`.

## What you get

| Feature | How it works |
|---|---|
| State | The extension reports turns, tool calls, file changes, retries, compaction and the end of a session. |
| Reconnect | The extension sends a snapshot of the live session each time it reconnects. Vibeke then shows the correct state after a server restart. |
| Dialogs | When an extension in pi asks you to confirm, choose or type, Vibeke shows an inbox card. A confirm dialog is an approval. A choose or type dialog is a question. You can answer the card, and pi closes its own dialog. In the pane that has focus, you can still answer in pi. |
| omp approvals | omp has its own tool approvals. The extension reports them as approvals in the inbox. |
| Usage | The extension reports tokens and cost for each message. Vibeke also reads the session file. |
| Model switching | `vibeke agent models <target>` lists models. `vibeke agent set-model <target> <model>` switches. See below. |
| Commands | `vibeke agent commands <target>` lists the commands of the live session, including those from extensions and skills. |
| Images | The extension adds a `show_image` tool. The agent can use it to attach an image file for you. |
| Resume | Vibeke can resume a pi session with `pi --session <id>` and an omp session with `omp --resume <id>`. |

The extension only observes. It never blocks or approves a tool call. pi has no permission system. If you want approvals in pi, install a permission extension of your choice. Vibeke then shows and answers the dialogs of that extension.

### Model switching

- pi saves every switch as the default model. Run `vibeke agent set-model <target> <model> --scope default`. The default scope, `session`, is refused and changes nothing.
- omp keeps a switch in the session. omp refuses `--scope default`.

## Install

1. Open **Setup** in the command palette. Select pi, omp or both. Confirm.
2. Or run the commands below. Without `--yes`, the command only prints the planned change.

```sh
vibeke integration install pi
vibeke integration install pi --yes
vibeke integration install omp --yes
```

3. Restart pi or omp, or run `/reload` in pi.

## Files it changes

| Agent | File |
|---|---|
| pi | `~/.pi/agent/extensions/vibeke/index.js` |
| omp | `~/.omp/agent/extensions/vibeke.js` |

The first line of the file marks it as managed by Vibeke. A reinstall overwrites it. If a file exists at that path without the mark, the installer refuses to replace it.

The extension stays inactive outside a Vibeke pane.

## Check

```sh
vibeke integration status pi
vibeke integration doctor omp
vibeke integration capabilities pi
```

`status` lists other extensions it finds in the same folder as `foreign`. `doctor` compares the agent version with the validated range.

## Settings

```toml
[agents.harness.pi]
integration = "extension"   # extension | rpc | screen
```

## Remove

```sh
vibeke integration uninstall pi --yes
vibeke integration uninstall omp --yes
```

This deletes the managed extension file. It refuses to delete a file that Vibeke did not write.

## Limits

- Answers to dialogs depend on pi internals. A pi version that Vibeke has not validated may not accept an answer from a card. Answer in the pane then.
- Vibeke does not look for background tasks that pi or omp report. It looks for shell commands that keep running after the turn.
- `show_image` and the dialog bridge need the extension to run inside a Vibeke pane.

## Next steps

- [Agents and interactions](../concepts/agents.md): requests, answers and background work.
- [Customize Vibeke](../customize.md): change settings and keys.
- [Troubleshooting](../troubleshooting.md): fix an extension that does not report.
