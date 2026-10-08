# Your first workspace

Install [Vibeke](install.md) and your preferred agent CLI. Authenticate the agent with its provider before starting.
Vibeke provides the workspace. Your agent uses its own provider account.

## Start in your project

In your terminal, change to your project directory and start Vibeke:

```sh
cd your-project
vibeke
```

Vibeke starts its server when needed. A fresh session creates a workspace in your current directory.
An existing session reconnects to its workspaces. Use **New workspace** in the command palette to add another project.

## Complete first-run setup

The setup screen checks your terminal and offers agent integrations, notifications, and a theme.
Select the integrations you use, review their changes, and confirm installation.
Save the configuration when you finish. You can reopen **Setup** from the command palette at any time.

Integrations report structured agent state and deliver supported answers. You can also use an ordinary shell without them.

## Start your agent

At the shell prompt inside a pane, run your usual agent command:

```sh
claude
```

Or run `codex`, `pi`, `omp`, or another installed agent CLI.
Keep using the agent as you normally would. The sidebar shows agent state when an integration or terminal detection is available.

## Find your way around

Press **Ctrl+B**, release both keys, then press the key in this table:

| Key | Action |
| --- | --- |
| `:` | Open the command palette. Type an action name and press Enter. |
| `?` | Show key help. |
| `v` | Split the pane side by side. |
| `c` | Create a tab. |
| `w` | Navigate workspaces in the sidebar. |
| `i` | Open the attention inbox. |
| `q` | Detach this terminal client. |

For an agent request, open the inbox, review the details, and send your answer.
Supported integrations deliver the answer through the agent's own interface.

See [using the terminal interface](terminal.md) for more controls.

## Disconnect and reconnect

Detach with **Ctrl+B**, then **q**, or close the terminal. Run `vibeke` again to reconnect.
Holder processes keep panes active after a client disconnect or server restart.
A host restart stops these processes. Screen recovery after a server restart can be incomplete.

See [process durability](concepts/holders.md).

## Next steps

- [Connect the desktop app](desktop.md) to your local or remote host.
- [Pair your phone or browser](mobile.md) to answer requests away from your terminal.
- Use **New task (git worktree)** in the command palette to create a separate checkout. See [tasks and review](concepts/tasks.md).
- [Connect through SSH](remote.md) to work on another machine.
- Use the [CLI reference](reference/cli.md) for scripting and automation.

Run `vibeke doctor` if something does not work. It checks your installation, terminal, sockets, and integrations.
