# Claude Code

Install the Claude Code integration to see Claude's state in the sidebar and to answer its requests from the inbox, from another pane, or from your phone.

## What you get

| Feature | How it works |
|---|---|
| State | Claude Code hooks report when a turn starts, when a tool runs, when the agent is idle, and when it fails or ends. The sidebar shows this state. |
| Permission requests | A permission request becomes an inbox card. Vibeke sends your answer back through the hook, so Claude continues without a keystroke. |
| Plan reviews | A plan review becomes a card. You answer it through the hook. |
| Questions | A question from Claude becomes a card. Vibeke answers it by sending keystrokes to the Claude prompt. This is best effort. |
| MCP input requests | The installer also adds the hook for an MCP server that asks you for input. |
| Background tasks | Claude reports its background shells and monitors. The sidebar shows them after `⧗`. See [Agents and interactions](../concepts/agents.md). |
| Usage and limits | Vibeke reads Claude's transcript file. It records tokens and cost for each turn and notes rate limits. Run `vibeke agent turn-usage <run>` or `vibeke agent limits`. |
| Resume | Vibeke starts Claude with a session ID. After a restart it can resume the session with `claude --resume`. |

Claude Code has no structured model switch in Vibeke. `vibeke agent models` reports that it is not supported. Type `/model` in the pane instead.

## Install

1. Open **Setup** in the command palette. Select Claude Code. Review the planned change. Confirm.
2. Or run the commands below. Without `--yes`, the command only prints the planned change.

```sh
vibeke integration install claude
vibeke integration install claude --yes
```

3. Start `claude` in a Vibeke pane. A session that already runs needs a restart to load new hooks.

## Files it changes

- `~/.claude/settings.json`, or `settings.json` in the directory named by `CLAUDE_CONFIG_DIR`.
- Vibeke adds one hook entry for each event it watches. Each entry runs `vibeke hook claude <Event>`. A `_vibeke` key marks the entries as its own.
- The installer keeps your other hooks and your other settings.
- Before the first change, it saves a copy named `settings.json.vibeke-bak-<timestamp>` next to the file.

Add `--mcp` to register the `vibeke mcp` server in `~/.claude.json` instead of the hooks.

## Check

```sh
vibeke integration status claude
vibeke integration doctor claude
vibeke integration capabilities claude
```

- `status` says `installed`, `partial` or `not installed`. For a partial install it lists the missing events.
- `doctor` finds the `claude` program, prints its version, and compares it with the validated range. It also shows the capabilities that apply.
- `capabilities` prints the same capability list for your version.
- These checks are local. To confirm that hooks reach the server, start `claude` in a Vibeke pane and check that the sidebar shows its state.

A Claude Code version outside the validated range gets `observe` and keystroke answers only. Vibeke still shows state. It does not send native answers until the version is validated.

## Settings

In `config.toml`:

```toml
[agents.approvals.claude]
persist_always = false   # true: "allow always" edits Claude's settings
persist_destination = "localSettings"
```

By default, an "allow always" answer applies only to the session. With `persist_always = true`, Vibeke writes the rule to the Claude settings file that `persist_destination` names. See the [configuration reference](../reference/config.md).

## Remove

```sh
vibeke integration uninstall claude --yes
```

This removes only the entries that Vibeke added. Your own hooks stay.

## Limits

- Hooks only report for Claude Code that runs in a Vibeke pane.
- A hook waits up to 30 minutes for your answer. After that, Vibeke gives no decision and Claude shows its own dialog.
- Vibeke answers Claude questions with keystrokes, not through a hook.
- Vibeke does not change Claude's model. Use `/model`.

## Next steps

- [Agents and interactions](../concepts/agents.md): how requests, answers and background work behave for every agent.
- [Run agents in parallel](../parallel-agents.md): start several Claude Code agents and track them.
- [Troubleshooting](../troubleshooting.md): fix hooks that do not report.
