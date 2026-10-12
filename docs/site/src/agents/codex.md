# Codex CLI

Install the Codex CLI integration to see Codex state in the sidebar and to answer approval requests from the inbox.

## What you get

| Feature | How it works |
|---|---|
| State | Codex hooks report turns, tool use, interrupts and the end of a session. |
| Approval requests | A Codex approval becomes an inbox card. Vibeke sends your answer back through the hook. |
| Other prompts | Vibeke answers other Codex prompts by sending keystrokes. This is best effort. |
| Usage and limits | Vibeke reads the Codex session file. It records tokens for each turn and the account limits that Codex reports. |
| Background tasks | Vibeke shows background terminals that Codex reports when it runs Codex through the Codex app server. See [Agents and interactions](../concepts/agents.md). |
| Resume | Vibeke can resume a session with `codex resume <id>`. |
| Model switching | `vibeke agent models` and `vibeke agent set-model` work for a Codex run that Vibeke starts through the Codex app server. In a normal pane, type `/model` in Codex. |

## Install

1. Open **Setup** in the command palette. Select Codex CLI. Review the planned change. Confirm.
2. Or run the commands below. Without `--yes`, the command only prints the planned change.

```sh
vibeke integration install codex
vibeke integration install codex --yes
```

3. Run `/hooks` in Codex once and trust the Vibeke hooks. Codex ignores hooks that you have not trusted.
4. Start `codex` in a Vibeke pane.

## Files it changes

- `~/.codex/hooks.json`, or `hooks.json` in the directory named by `CODEX_HOME`. Each Vibeke entry runs `vibeke hook codex <Event>`. Vibeke finds its own entries by that command and keeps your other hooks.
- Before the first change, it saves a copy named `hooks.json.vibeke-bak-<timestamp>`.
- A small `codex` script in the `shims` folder of the Vibeke data directory. Panes put this folder first in `PATH` while `agents.shims` is `true`. The script runs your real `codex` with `--disable daemon_auto_start`. Set `CODEX_VIBEKE_SHIM=0` to skip the script for one command.
- `~/.agents/skills/vibeke/SKILL.md` holds the Vibeke skill. It teaches Codex the `vibeke` commands for panes, agents, tasks, previews, the agent browser and images. pi, oh-my-pi, OpenCode and Gemini CLI read the same file. A line after the header of the skill marks it as managed by Vibeke. A reinstall overwrites it. The installer does not replace a `vibeke` skill without that line.

The installer prints the shim path when it writes the script.

The `--mcp` flag registers the `vibeke mcp` server in `~/.codex/config.toml`. It writes only that entry.

## Check

```sh
vibeke integration status codex
vibeke integration doctor codex
vibeke integration capabilities codex
```

- `status` shows how many hooks Codex has not trusted yet. It also says when `config.toml` sets `features.hooks = false`, which makes Codex ignore `hooks.json`.
- `doctor` prints the Codex version and compares it with the validated range.

A Codex version outside the validated range gets `observe` and keystroke answers only.

## Remove

```sh
vibeke integration uninstall codex --yes
```

This removes the Vibeke hooks from `hooks.json`. It does not delete the `codex` script in the shims folder. To stop using the script, set `agents.shims = false` in `config.toml`, or delete the script.

## Limits

- The tool hook watches only `Bash` tool calls.
- Codex has no structured model switch in a normal pane.
- Hooks only report for Codex that runs in a Vibeke pane.

## Next steps

- [Agents and interactions](../concepts/agents.md): requests, answers and background work.
- [Run agents in parallel](../parallel-agents.md): start several agents and track them.
- [Troubleshooting](../troubleshooting.md): fix hooks that do not report.
