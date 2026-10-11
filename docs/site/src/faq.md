# Frequently asked questions

## How is Vibeke different from running agents in tmux?

tmux keeps terminals alive. Vibeke also understands the agents in them. It shows each agent's state in the sidebar and collects permission requests and questions in one inbox. It gives each task its own git worktree, and it lets you answer from your phone. Pane processes also survive a server restart. See the [introduction](introduction.md) and [agents and interactions](concepts/agents.md).

## How is Vibeke different from Herdr?

Herdr is another terminal workspace for coding agents. Both tools run in the terminal you already use. Both keep agents running in a background server after you close the terminal. Both mark each agent as working, blocked or done. Both use **Ctrl+B** as the prefix, the key you press before a shortcut.

Vibeke adds these features:

- **Panes survive a server restart.** A separate holder process keeps each pane running when the server stops or updates. Herdr restores the layout after a restart. The original processes stop. Herdr can keep processes running during an update with live handoff. That feature is experimental. See [process durability](concepts/holders.md).
- **An inbox for approvals.** Permission requests and questions from agents collect in one list. Vibeke shows the requests that an integration reports or that it recognizes on the screen. You answer in the inbox. Vibeke then sends the answer to the agent. In Herdr, you answer in the agent's pane. See [agents and interactions](concepts/agents.md).
- **Phone and browser access without SSH.** SSH is a program that opens a terminal on another computer. Herdr uses an SSH client on the phone. In Vibeke, you pair a phone once. It connects through a relay. The relay is a server that forwards encrypted traffic. It cannot read that traffic. The host needs no open ports. The browser app shows approvals and sends push notifications. See [phone and browser access](mobile.md).
- **Tasks with review.** A task stores its goal and success criteria. A new task can get its own git worktree, which is a separate copy of the repository with its own branch. You can review the diff, run checks and accept the result. Herdr creates worktrees and can start a review agent. It does not store tasks or check results. See [tasks and review](concepts/tasks.md).
- **Isolation.** A task can run in a sandbox or a container, which limits what the agent can reach on the host. Vibeke refuses to start the task when that isolation is not available. Herdr runs agents with your user permissions unless you start them in your own sandbox tool. See [execution levels](concepts/sandboxes.md).
- **Moving and sharing work.** You can hand work to another host or to a teammate. You can invite someone to watch or approve a pane for a limited time. See [move work to another host](handoff.md) and [share with a teammate](sharing.md).
- **A browser pane.** An agent can control a Chromium browser pane through the Vibeke API. You can watch it and take control.
- **A desktop app** for macOS, Linux and Windows. See [desktop app](desktop.md).

Herdr has strengths too:

- The Herdr host runs natively on Windows. The Vibeke host runs on macOS and Linux only.
- Herdr detects more agent programs. It updates its detection rules from its website.
- After a computer restart, Herdr restores the saved layout. It can also resume supported agent sessions.
- Herdr has a plugin marketplace. Vibeke has partial support for Herdr plugins.

To move from Herdr, see [import from Herdr](migrating-from-herdr.md). The [comparison page](https://vibeke.dev/compare) compares more tools.

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
