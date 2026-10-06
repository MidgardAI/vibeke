# Quickstart

## Local

```sh
vibeke                      # attach the TUI; the server starts if needed
vibeke workspace create .   # from another shell: a workspace for the current directory
vibeke pane list
vibeke agent start claude   # run an agent in the focused pane
```

Detach by closing the terminal; panes keep running in their holders. `vibeke` attaches again. See [Sessions, workspaces, tabs and panes](concepts/layout.md).

## Over SSH

```sh
vibeke ssh devbox
```

`vibeke ssh <host>` attaches to a remote machine. On first use it pushes a verified `vibeke` build to the remote (from `~/.cache/vibeke/releases/<version>/`), then bridges the render stream over the SSH connection. Previews and forwarded ports stay on loopback. Remote machines are untrusted input: they cannot run local commands, and clipboard, open-URL and notification requests are gated (see the [security model](security.md)).

Manage machines with `vibeke machine add|list|connect|disconnect|status|remove`.

## Scripting

Every command maps to a control-API method and prints JSON. `vibeke api methods` lists what the running server offers, and `vibeke --skill` prints the embedded skill that teaches an agent to use the CLI. The full list is in the [CLI reference](reference/cli.md).
