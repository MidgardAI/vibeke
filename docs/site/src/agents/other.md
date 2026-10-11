# Other agents

Use this page to track an agent that has no dedicated integration. You can rely on terminal detection, describe your own agent in a manifest, or run an agent through the Agent Client Protocol (ACP).

For the agents with an integration, see [Claude Code](claude-code.md), [Codex CLI](codex.md), [pi and oh-my-pi](pi.md), [OpenCode](opencode.md) and [Gemini CLI](gemini.md).

## Terminal detection

Vibeke ships a manifest for these agents without an integration:

Aider, Amp, Antigravity, Cursor Agent, Devin CLI, Factory Droid, GitHub Copilot CLI, Grok CLI, Hermes Agent, Kilo Code CLI, Kimi CLI, Letta Code, Mastra Code, Muse, Qoder CLI and Qwen Code.

Run the agent in a pane. Vibeke finds the process, reads the screen and shows a state in the sidebar. Where the manifest has a rule for the approval prompt, Vibeke also shows the prompt in the inbox and answers it with keystrokes.

- No setup is needed. `vibeke integration install` has nothing to install for these agents.
- The screen rules are best effort. Vibeke has not checked them against a live release of these agents. A new version of the agent can break detection.
- Vibeke shows background work as the shell commands that the agent started and that still run after its turn.
- Vibeke does not read transcripts or usage for these agents.

Run `vibeke integration list` to see every manifest. Run `vibeke integration doctor all` to see which agent programs Vibeke finds and which capabilities apply.

## Custom manifests

A manifest is a TOML file that tells Vibeke how to find an agent and how to start it. Use it for a wrapper script around an agent that Vibeke already knows, or for an agent that Vibeke does not know.

1. Create `~/.config/vibeke/harnesses/my-agent.toml`. The folder is `harnesses` next to your `config.toml`.
2. Describe the agent. This example adds a wrapper around pi:

```toml
schema = 1
id = "my-pi"
name = "pi (my wrapper)"
extends = "pi"                 # inherit transports, capabilities and screen rules

[detect]
priority = 200                 # beat pi itself, which runs under the wrapper
[[detect.process]]
argv_regex = '(^|/)my-pi( |$)'

[launch]
argv = ["my-pi"]
argv_with_session = ["my-pi", "--session-id", "{session_id}"]

[resume]
argv = ["my-pi", "--session", "{resume_id}"]
```

3. Reload the manifests. A restarted server loads them. A running server reloads them with:

```sh
vibeke api call agent.manifests_reload
```

4. Check the result:

```sh
vibeke integration list --sources
vibeke integration doctor my-pi
```

`list --sources` shows where each manifest came from: built in, the update channel, your user folder, or a repository.

Notes:

- A user manifest with the ID of a built-in manifest merges over it. Tables merge. Lists replace.
- A manifest with `extends` inherits the transports, capabilities and screen rules of its base.
- A repository can ship manifests in `.vibeke/harnesses/`. Vibeke loads them only for a repository that you trust. Their IDs start with `repo:`, and they cannot replace another manifest.

## Agent Client Protocol (ACP)

ACP is a protocol that lets a program control an agent over standard input and output. Vibeke can run an agent that speaks ACP.

Vibeke starts `vibeke acp-host` in the pane. The host runs the agent, shows a minimal conversation in the pane and sends state to Vibeke.

Use the harness ID `acp:<name>`:

```sh
vibeke agent start review --harness acp:gemini
vibeke agent start review --harness acp:my-agent --args my-agent,--acp
```

- For a name that has a manifest with an ACP command, such as `gemini` or `opencode`, Vibeke knows the command. For another name, pass the command in `--args`.
- A permission request from the agent becomes an inbox card. Vibeke sends your answer through the protocol. You can also answer in the pane.
- Interrupting the agent cancels the turn through the protocol.
- The host serves file reads and writes inside the working directory of the session. It does not offer terminals to the agent.
- The agent reports usage only when it adds it to its prompt result.

Vibeke tested this support with a test agent. It has not been checked against live third-party agents. The ACP command line for Gemini CLI, OpenCode and Hermes Agent is unverified.

## Update the manifest list

Vibeke can fetch newer manifests from a signed update channel. The command refuses an index without a valid signature.

```sh
vibeke integration update
vibeke integration pin <id> current
vibeke integration unpin <id>
```

- `update` applies the manifests that are new or changed. It prints the ones it skipped.
- `pin <id> [<version>|current]` freezes a fetched manifest. Updates then leave it as it is.
- `unpin <id>` lets the next update replace it.
- A running server picks up the result after a restart or after `vibeke api call agent.manifests_reload`.

## Limits

- Vibeke does not write any file for an agent without an integration.
- Detection by screen cannot answer a prompt that it does not recognize. Use the pane then.

## Next steps

- [Agents and interactions](../concepts/agents.md): requests, answers and background work.
- [Customize Vibeke](../customize.md): change settings and keys.
- [Troubleshooting](../troubleshooting.md): fix an agent that Vibeke does not detect.
