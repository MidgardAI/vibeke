# Using the terminal interface

Vibeke opens into a shell pane. Run your usual commands and agent CLIs there.
Use the command palette and sidebar to manage the surrounding workspace.

## Open the command palette

Press **Ctrl+B**, release both keys, then press **:**.
Type an action name, choose a result, and press Enter. Press Escape to close the palette.

Useful actions include **New workspace**, **New tab**, **Split side by side**, **Attention inbox**, and **Setup**.
The palette displays configured shortcuts beside actions.

## Workspaces, tabs, and panes

A workspace groups a project. Tabs hold layouts, and panes run shells or agents.
A fresh session creates its first workspace in the directory where you start Vibeke.

Use **New workspace** for another project. Use **New git worktree** when you want a separate checkout.
Use **New task (git worktree)** when you also want task tracking and review.

These are the default keys. Press **Ctrl+B** before each key:

| Key | Action |
| --- | --- |
| `w` | Navigate the sidebar. |
| `g` | Find a workspace, tab, agent, or task. |
| `c` | Create a tab. |
| `n` / `p` | Next / previous tab. |
| `v` / `-` | Split side by side / stacked. |
| `h` / `j` / `k` / `l` | Focus left / down / up / right. |
| `z` | Zoom the current pane. |
| `i` | Open the attention inbox. |
| `a` | Find the next agent that needs attention. |
| `s` | Open setup and settings. |
| `q` | Detach this client. |
| `?` | Show key help. |

Your configuration can change these keys. Run `vibeke keys` to see active bindings.

## Setup and integrations

First-run setup checks terminal support and offers integrations for detected agents.
It also configures notifications and your theme. Existing Herdr configuration can be imported when detected.

Select integrations with Space. Press **d** to inspect changes, then **i** to install selected integrations.
Confirm the listed files with **y**. Save the configuration at the final step.

Reopen **Setup** from the palette to change these choices.
For troubleshooting, use `vibeke integration status all` and `vibeke doctor`.

## Agent requests

Open **Attention inbox** to review permission requests, questions, and plan reviews.
Select the request, read its details, and answer it. Delivery depends on the agent integration.
See [agents and interactions](concepts/agents.md).

## Phone and desktop access

The desktop app has a **Connect to this Mac/computer** flow.
**Connections** (`prefix` then `alt+d`) has four tabs, switched with `tab` and `shift+tab`: **Devices** pairs a phone or browser (also **Pair a phone** in the command palette) and lists and revokes paired devices; **People** lists, creates and revokes colleagues' shared access to a pane or workspace; **Hosts** manages your paired hosts and handoff invitations; **Handoffs** lists incoming handoffs (`prefix+shift+h`).

See [desktop access](desktop.md) and [phone and browser access](mobile.md).
