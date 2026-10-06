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

You do not need to open the agent pane. The integration sends your answer through the agent's native interface when available.

Vibeke stores the decision, delivery attempt, and acknowledgment. The request and delivery state remain after a server restart. Repeated answer IDs do not send the answer twice.

An agent can read its own requests. It cannot answer them.

## Configure an integration

Use `vibeke integration install|status|uninstall|doctor` to manage integrations. The installer preserves existing hooks.

Use `vibeke agent start|prompt|wait|read|send-keys|get|list` to control agent runs. See the [CLI reference](../reference/cli.md) for command arguments.
