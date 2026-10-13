# Using the terminal interface

Vibeke opens into a shell pane. Run your usual commands and agent CLIs there.
Use the command palette and sidebar to manage the surrounding workspace.

## Open the command palette

Press **Ctrl+B**, release both keys, then press **:**.
Type an action name, choose a result, and press Enter. Press Escape to close the palette.

Useful actions include **New workspace**, **New tab**, **Split side by side**, **Attention inbox**, and **Setup**.
The palette displays configured shortcuts beside actions.

## See every key

Press **Ctrl+B** and wait a moment. A menu lists every prefix key in groups: pane, tab, workspace, agents, and session, plus your own commands and plugin actions.
Press a key to run it, or Escape to close the menu. It stays open until you do one of those.
Press **?** after **Ctrl+B** to open it at once. Resize mode shows its keys the same way.

Set `prefix_menu_ms` under `[keys]` to change the delay, or `prefix_menu = false` to turn the menu off.

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
| `@` | Name panes and mention them. See [Mention other agents](mentions.md). |
| `?` | Show the key menu. |

Your configuration can change these keys. Run `vibeke keys` to see active bindings. See [customize keys and appearance](customize.md).

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

## Images

Press `prefix+alt+i` to view new images. The sidebar also lists recent images. See [previews and browsers](concepts/previews.md#images-in-the-terminal-interface).

## Keys in forms and tabbed views

Every form, dialog and tabbed view uses the same keys:

| Key | What it does |
| --- | --- |
| `left`, `right` | Change the selected option: the previous or next value, or a toggle on or off |
| `space` | Change the selected option too |
| `up`, `down`, `tab`, `shift+tab` | Move between the fields |
| `enter` | Confirm |
| `esc` | Go back |

In a view with tabs, `tab` and `shift+tab` switch tabs from a list. In a form, they move between the fields instead. To switch tabs from a form, press `up` from the top row. The tab strip is then selected: use `left` and `right` to switch tabs, and `down` to go back. The letter shortcuts that a view shows still work.

## Drafts, notes and the session desk

**Drafts and Notes** (`:drafts`, `:notes` in the command palette) are two tabs. **Results and Sessions** in the session desk (`:desk`) are two tabs too. Switch tabs with `tab` and `shift+tab`. You can also press `up` from the top row, then use `left` and `right`. Press `down`, `enter` or `esc` to go back into the tab.

When you send a draft (`s`), the send view has two fields: the list of targets and **include notes**. Press `tab` or `shift+tab` to move between them. On the list, `up` and `down` pick a target. On **include notes**, `left`, `right` and `space` turn it on or off. The `i` key still works.

The desk shows three filter chips: repository, harness and date. Press `up` from the top result to select the chips. Use `left` and `right` (or `space`) to change the selected chip's value, and `tab` or `shift+tab` to move between the chips. Press `down` or `enter` to go back to the results. The keys `R`, `h` and `D` still change the chips. When you resume a session (`r`), choose the target with `left`, `right`, `up` or `down`, then press `enter`. The keys `t` and `p` still work.

## Phone and desktop access

The desktop app has a **Connect to this Mac/computer** flow.
**Connections** (`prefix` then `alt+d`) has four tabs. Switch them with `tab` and `shift+tab`, or press `up` from the top of a list or form and then `left` and `right`. In a form, a picker or a prompt, such as the choice of what a new phone may do, `left` and `right` (or `space`) change the selected option. There, `tab`, `shift+tab`, `up` and `down` move between the fields, and `tab` does not switch tabs. The tabs are: **Devices** pairs a phone or browser (also **Pair a phone** in the command palette) and lists and revokes paired devices; **People** lists, creates and revokes colleagues' shared access to a pane or workspace; **Hosts** manages your paired hosts and handoff invitations; **Handoffs** lists incoming handoffs (`prefix+shift+h`).

**Setup** (the **Setup** action, or `:setup`) walks through the first-run steps. Switch steps with `tab` and `shift+tab`, or press `up` from the top of a step and then `left` and `right`. In a list, `left` and `right` change the selected option: they choose the previous or next notification channel or theme, and they turn an agent integration off or on. `space` also changes the selected option. Press `esc` to leave Setup for now.

See [desktop access](desktop.md) and [phone and browser access](mobile.md).

## Next steps

- [Customize keys and appearance](customize.md): change keys, theme, and bars.
- [Run agents in parallel](parallel-agents.md): work on one repository with several agents.
- [Workspaces, tabs, and panes](concepts/layout.md): learn how the layout fits together.
