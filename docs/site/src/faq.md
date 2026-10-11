# Frequently asked questions

## How is Vibeke different from running agents in tmux?

tmux keeps terminals alive. Vibeke also understands the agents in them. It shows each agent's state in the sidebar and collects permission requests and questions in one inbox. It gives each task its own git worktree, and it lets you answer from your phone. Pane processes also survive a server restart. See the [introduction](introduction.md) and [agents and interactions](concepts/agents.md).

## Does Vibeke send my code anywhere?

Your code stays on your host unless you move it.

- Agents talk to their own providers, as they do outside Vibeke.
- Phones and other devices connect through a relay. The relay forwards encrypted traffic and cannot read it. See [phone and browser access](mobile.md).
- A [cloud sandbox](cloud-sandboxes.md) copies your task to the provider you choose.
- A [handoff](handoff.md) sends work to another host that you or a teammate paired.
- Telemetry is disabled. See the [security model](security.md).

## Is it safe to let agents run on my host?

In host mode, an agent runs with your user permissions. Pane tokens and scopes limit what an agent can do through the Vibeke API. They are a guardrail, not a security boundary. For a real boundary, run the task at the `sandbox`, `container` or `vm` execution level. Vibeke refuses to start the process when the isolation you chose is not available. See [execution levels](concepts/sandboxes.md) and the [security model](security.md).

## Which agents work?

Vibeke has integrations for Claude Code, Codex, pi and omp. Run `vibeke integration list` to see them. Other agents run in a pane too. For those, Vibeke reads the terminal output, which shows less detail. See [agents and interactions](concepts/agents.md).

## Which platforms does it support?

The host CLI runs on macOS with Apple silicon and on Linux with x86_64 or aarch64 processors. The desktop app runs on macOS, Linux and Windows, and connects to a host. The [browser app](mobile.md) works on a phone or a computer. See [installation](install.md).

## Does it cost anything or need an account?

You can install and use the terminal interface without an account. Phone access uses a relay. The relay `relay.vibeke.dev` accepts any host without an account. The relay `cloud.vibeke.dev` needs a Vibeke account, which you create with `vibeke login`. You can also [run your own relay](self-hosting.md).

A cloud sandbox needs an account with a sandbox provider. You sign in to the provider with your own token. Your agents also need their own logins with their providers. Vibeke does not move those logins to another host.

## What happens when I close the terminal or restart?

Closing the terminal or detaching leaves your panes running. Run `vibeke` again to reconnect. A server restart, including an update, also keeps panes running. A restart of the computer stops them. See [process durability](concepts/holders.md).

## Can I use my configuration from Herdr?

Yes. First-run setup offers to import it. You can also run `vibeke import herdr --dry-run` to preview the import. See [import from Herdr](migrating-from-herdr.md).

## How do I update or uninstall?

To update, run `vibeke update --check` and then `vibeke update`. Use `vibeke update --rollback` to return to the previous version. See [update Vibeke](install.md#update-vibeke).

To uninstall the CLI, stop the server with `vibeke server stop --kill-panes`. Then remove `~/.local/bin/vibeke` and the `~/.local/share/vibeke/versions` directory. Your configuration is in `~/.config/vibeke/config.toml`.

## Next steps

- [Your first workspace](quickstart.md) to start using Vibeke.
- [Troubleshooting](troubleshooting.md) if something does not work.
- [Security model](security.md) for the full list of threats and controls.
