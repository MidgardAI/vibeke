# Agents and interactions

Vibeke uses these structured integrations:

- Claude Code hooks and transcripts.
- The pi/omp extension.
- The Codex app server through a command wrapper in `PATH`.

If structured state is unavailable, Vibeke can read terminal output. Each `AgentState` includes the information source and confidence.

## Answer a request

An **interaction** is a permission request, question, or plan review from an agent. The inbox shows each interaction as a card.

1. Open the request in the inbox.
2. Read the request details.
3. Select or enter your answer.
4. Send the answer.

The integration sends your answer through the agent's native interface when available.

Vibeke stores the decision, delivery attempt, and acknowledgment. The request and delivery state remain after a server restart. Repeated answer IDs do not send the answer twice.

An agent can read its own requests. It cannot answer them.

## Background work

An agent can leave work running after its turn ends. Examples are a development server, a background shell command, or a watcher. Vibeke then does not show the agent as done.

- The sidebar shows `⧗` and a label such as `idle · 2 bg`.
- The status bar counts background tasks after `⧗`.
- The agent details list each task with its kind, its command, and how long it has run.
- The apps show the tasks next to the agent and in the conversation.

Vibeke finds background work in these ways:

- **Claude Code** reports its background tasks, including background subagents and monitors.
- **Codex** records background terminals and sub-agents in its session file. A sub-agent counts from when it starts until it finishes or is closed.
- **oh-my-pi** async jobs (`task` subagents and background `bash`) and **pi** runs from the `pi-subagents` package are reported by the Vibeke extension.
- **Other agents:** Vibeke looks for shell commands that the agent started with a tool call and that still run after the turn. Servers that start together with the agent, such as MCP servers, do not count. Shell commands that you start yourself do not count.

## Configure an integration

Open **Setup** in the command palette to select integrations, inspect changes, and confirm installation. The installer preserves existing hooks.

An integration has two parts. The hooks or the extension report the agent state to Vibeke and let Vibeke answer approvals. The Vibeke skill teaches the agent the `vibeke` commands, for example to declare a preview, check a page in the agent browser or show you an image.

Setup changes only the computer where it runs. For a remote host, see [Work on a remote machine](../remote.md#agent-integrations-on-the-host).

### Updates

After you update Vibeke, the next `vibeke` command that opens the terminal interface checks the installed integrations once:

- Vibeke replaces an old copy of the skill or of an extension without a question. Vibeke manages these files completely.
- If the hooks need a change, Vibeke asks first. Hooks need a change when a new release adds an event, when the skill is missing, or when the hooks call a `vibeke` program that no longer exists. The hooks live in the configuration files of your agents. Press Enter or `y` to update them. If you answer `n`, Vibeke asks again after the next update.

Restart a running agent to load the new hooks. After a Codex hook update, run `/hooks` in Codex once to trust the new hooks.

Then run your usual `claude`, `codex`, `pi`, or `omp` command in a shell pane.
For scripts and troubleshooting, use `vibeke integration install|status|uninstall|doctor`.

Each agent has its own page:

- [Claude Code](../agents/claude-code.md)
- [Codex CLI](../agents/codex.md)
- [pi and oh-my-pi](../agents/pi.md)
- [OpenCode](../agents/opencode.md)
- [Hermes Agent](../agents/hermes.md)
- [Other agents](../agents/other.md): terminal detection, custom manifests and ACP agents

Use `vibeke agent start|prompt|wait|read|send-keys|get|list` to control agent runs. See the [CLI reference](../reference/cli.md) for command arguments.

## Next steps

- [Run agents in parallel](../parallel-agents.md): run several agents and find the one that needs you.
- [Tasks and review](tasks.md): give an agent its own worktree.
- [Execution levels](sandboxes.md): limit what an agent can reach.
