# Quickstart

## Before you start

1. [Install Vibeke](install.md).
2. Install your agent CLI.
3. Authenticate the agent with its provider.

Vibeke provides the workspace. Your agent uses its own provider account.

## Open a workspace

1. Start Vibeke:

   ```sh
   vibeke
   ```

   Vibeke starts the server if necessary.

2. Open another shell.
3. Change to your project directory.
4. Create a workspace:

   ```sh
   vibeke workspace create .
   ```

5. List the panes:

   ```sh
   vibeke pane list
   ```

6. Start an agent:

   ```sh
   vibeke agent start builder --harness claude
   ```

`builder` is the agent name. `--harness claude` selects the agent integration. To select a pane, add `--pane` with a handle from `vibeke pane list`.

The target pane must have an available shell prompt.

## Read agent state

```sh
vibeke agent list --json
vibeke interaction list --status open
```

An **interaction** is a request from an agent. Examples include permission requests and questions. Install the applicable [agent integration](concepts/agents.md) for structured state and request delivery.

## Disconnect and reconnect

1. Close the terminal to disconnect.
2. Run `vibeke` again to reconnect.

Holder processes keep panes active after a client disconnect or server restart. A host restart stops these processes. Screen recovery after a server restart can be incomplete.

See [process recovery](concepts/holders.md).

## Create an isolated task

From your repository, create a task worktree:

```sh
vibeke task new "Review the session recovery path" --repo .
vibeke task list
```

A worktree separates file changes. Host-mode tasks still use your user permissions. See [execution levels](concepts/sandboxes.md) for isolation options.

## Connect through SSH

Before the first connection, read [release verification](reference/releases.md). Remote installation currently requires explicit permission to use unsigned files.

```sh
vibeke ssh devbox
```

On first use, Vibeke checks a local release file and uploads it to the host. It uses `~/.cache/vibeke/releases/<version>/` by default. Terminal data then passes through SSH.

Previews and forwarded ports use loopback addresses. Remote requests cannot directly run local commands. Clipboard access, URL requests, and notifications have separate controls.

See the [security model](security.md). Use `vibeke machine add|list|connect|disconnect|status|remove` to manage hosts.

## Use scripts

CLI commands call the control API. Use `--json` for JSON output. `vibeke api methods` lists the server methods. `vibeke --skill` prints agent instructions for the CLI.

See the [CLI reference](reference/cli.md).

## Diagnose a problem

Run `vibeke doctor` to check the installation, terminal, sockets, and integrations. Use `vibeke agent harnesses` to list available agent integrations.

If a command fails, check its arguments in the CLI reference. The control API can change before version 1.0.
