# Vibeke

Vibeke is a terminal workspace for coding agents. It connects to agent hooks, extensions, and RPC interfaces. It can also detect agent state from terminal output.

Separate holder processes keep agents active after a server restart. Task worktrees separate file changes. Remote previews let you open an app from another host.

## Who Vibeke is for

Vibeke is for people who run several coding agents at the same time. You may use Claude Code, Codex, pi or another agent in a terminal.

Vibeke helps with four problems:

- **Many agents in one place.** Each agent runs in its own pane. The sidebar shows which agents work, wait or finish.
- **Agents that keep running.** An agent keeps running when you close the terminal or restart the server.
- **Requests from anywhere.** An agent can ask for permission or ask a question. You answer in the terminal, the desktop app, or the browser app on your phone.
- **Review of the work.** A task gives an agent its own git worktree. A worktree is a separate checkout of your repository. You read the changes before you merge them.

If you run one agent in one terminal and never leave your desk, you may not need Vibeke.

## How the parts fit together

![Vibeke architecture: clients, the server and holders on your host, remote machines, cloud sandboxes, and the relay](images/architecture.svg)

- The **server** runs on your laptop or devbox. It keeps workspaces, tasks, agent state, previews and screenshots.
- The **terminal interface** and the **CLI** connect to the server through a local socket.
- Each pane has its own **holder** process. The holder runs the agent. The agent keeps running when the server restarts.
- Agents report their state to the server through hooks, extensions and RPC. RPC means remote procedure call.
- The server can also reach other places. You can [connect a remote machine](remote.md) over SSH. You can run a task in a [cloud sandbox](cloud-sandboxes.md). You can [hand off](handoff.md) a running agent to another machine.
- From other devices, the desktop app, the browser app, your phone and your teammates connect through a [relay](mobile.md). The relay only sees encrypted traffic. Your host does not need open ports.
- On the same computer, the [desktop app](desktop.md) connects to the host directly.

Use this guide to install and operate Vibeke.

The control API is `vibeke/1`. See the [API reference](reference/api.md).

## Next steps

- [Start your first workspace](quickstart.md) after you install Vibeke.
- [Run your first task](first-task.md) in a separate git worktree.
- [Answer agents from your phone](phone-quickstart.md).
