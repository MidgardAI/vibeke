# Agents and interactions

Vibeke uses these structured integrations:

- Claude Code hooks and transcripts.
- The pi/omp extension.
- The Codex app server through a command wrapper in `PATH`.

OpenCode, Gemini CLI and other agents that speak the Agent Client Protocol (ACP) are experimental. You can also describe your own wrapper in a TOML manifest. Vibeke has not verified these against every version.

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

## Close a request without answering

Dismiss a request when you do not want to answer it. You can dismiss from the inbox, the sidebar, the terminal interface and the apps. Dismiss only hides the request in Vibeke. It does not send an answer to the agent. A dialog that you dismissed stays dismissed for the rest of the turn.

Vibeke also closes some requests by itself. It records each of these as closed automatically, not as answered:

- You start a new turn in the pane.
- Another tool starts in the same turn. Vibeke waits 2 seconds first, so a short delay does not close a request.
- The dialog is gone from the pane for 30 seconds. Vibeke checks this only for versions of an agent that it has validated.

Vibeke never closes a gated request or a sandbox request by itself. These stay open until you answer or dismiss them.

## Background work

An agent can leave work running after its turn ends. Examples are a development server, a background shell command, or a watcher. Vibeke then does not show the agent as done.

- The sidebar shows `⧗` and a label such as `idle · 2 bg`.
- The status bar counts background tasks after `⧗`.
- The agent details list each task with its kind, its command, and how long it has run.
- The apps show the tasks next to the agent and in the conversation.

Claude Code and the Codex app server report their background tasks to Vibeke. For other agents, Vibeke looks for shell commands that the agent started and that still run after the turn.

## Configure an integration

Open **Setup** in the command palette to select integrations, inspect changes, and confirm installation. The installer preserves existing hooks.

Then run your usual `claude`, `codex`, `pi`, or `omp` command in a shell pane.
For scripts and troubleshooting, use `vibeke integration install|status|uninstall|doctor`.

Use `vibeke agent start|prompt|wait|read|send-keys|get|list` to control agent runs. See the [CLI reference](../reference/cli.md) for command arguments.
