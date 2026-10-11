# Gemini CLI

Install the Gemini CLI hooks to see Gemini state in the sidebar and to find its confirmation prompts in the inbox.

The Gemini CLI integration has not been checked against a live Gemini CLI release. Vibeke names no validated version. Every version runs with the basic capabilities. Read the limits below before you rely on it.

## What you get

| Feature | How it works |
|---|---|
| State | Hooks report when a session starts, an agent turn starts and ends, a tool runs and a session ends. |
| Confirmation prompts | A Gemini notification and the confirmation screen become an inbox card. |
| Answers | Vibeke answers a confirmation by sending keystrokes to the pane. This is best effort. |
| Tool blocking | The tool hook can deny a tool call. It cannot approve one. |

Vibeke does not record usage or read a transcript for Gemini CLI.

## Install

1. Open **Setup** in the command palette. Select Gemini CLI. Review the planned change. Confirm.
2. Or run the commands below. Without `--yes`, the command only prints the planned change.

```sh
vibeke integration install gemini
vibeke integration install gemini --yes
```

3. Start `gemini` in a Vibeke pane. Run `/hooks` in Gemini and check that the Vibeke entries appear. Your Gemini settings may need hooks to be enabled.

## Files it changes

- `~/.gemini/settings.json`. Each Vibeke entry runs `vibeke hook gemini <Event>`. Vibeke finds its entries by that command and keeps your other settings and hooks.
- Before the first change, it saves a copy named `settings.json.vibeke-bak-<timestamp>`.

## Check

```sh
vibeke integration status gemini
vibeke integration doctor gemini
vibeke integration capabilities gemini
```

`doctor` prints the Gemini CLI version. It shows `observe`, `answer_keystroke` and the other documented capabilities that are unverified.

## Remove

```sh
vibeke integration uninstall gemini --yes
```

This removes only the Vibeke entries.

## Limits

- Hook names and the settings format follow the Gemini CLI documentation. Vibeke has not exercised them against a live release.
- Gemini payloads carry no tool call ID. Vibeke makes one up for each tool call in a pane.
- Vibeke has no structured model switch for Gemini CLI.
- Without hooks, Vibeke can still detect Gemini from the terminal screen. You can also run Gemini as an ACP agent. See [Other agents](other.md).

## Next steps

- [Other agents](other.md): run Gemini CLI as an ACP agent.
- [Agents and interactions](../concepts/agents.md): requests, answers and background work.
- [Troubleshooting](../troubleshooting.md): fix hooks that do not report.
