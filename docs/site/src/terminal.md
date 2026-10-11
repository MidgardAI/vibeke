# Using the terminal interface

Vibeke opens into a shell pane. Run your usual commands and agent CLIs there.
Use the command palette and sidebar to manage the surrounding workspace.

## The prefix key

Most shortcuts start with the **prefix key**. The default prefix is **Ctrl+Q**.
Press the prefix, release both keys, then press the next key. For example, press **Ctrl+Q**, release, then press **c** to create a tab.

The prefix is Ctrl+Q because shells, tmux, and agent CLIs already use Ctrl+B. No default shortcut uses the Alt or Option key. Option does not work as Meta in the default macOS terminals, and it types symbols on many keyboard layouts.

To send Ctrl+Q itself to the program in the pane, press the prefix twice. This is the `prefix_passthrough` setting under `[keys]`. It is on by default.

To use another prefix, set `prefix` under `[keys]`, for example `prefix = "ctrl+a"`. Run `vibeke keys` to see every active binding.

## Open the command palette

Press **Ctrl+Q**, release both keys, then press **:**.
Type an action name, choose a result, and press Enter. Press Escape to close the palette.

Useful actions include **New workspace**, **New tab**, **Split side by side**, **Attention inbox**, and **Setup**.
The palette displays configured shortcuts beside actions.

## See every key

Press **Ctrl+Q** and wait a moment. A menu lists every prefix key in groups: pane, tab, workspace, agents, and session, plus your own commands and plugin actions.
Press a key to run it, or Escape to close the menu. The menu also closes by itself after ten seconds without a key.
Press **?** after **Ctrl+Q** to open it at once. Resize mode shows its keys the same way.

Set `prefix_menu_ms` under `[keys]` to change the delay, or `prefix_menu = false` to turn the menu off.

## Workspaces, tabs, and panes

A workspace groups a project. Tabs hold layouts, and panes run shells or agents.
A fresh session creates its first workspace in the directory where you start Vibeke.

Use **New workspace** for another project. Use **New git worktree** when you want a separate checkout.
Use **New task (git worktree)** when you also want task tracking and review.

These are the default keys. Press **Ctrl+Q** before each key:

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
| `s` | Show where the settings file is. Use **Setup** in the palette to repeat the first-run steps. |
| `q` | Detach this client. |
| `[` | Enter copy mode. |
| `/` | Search the scrollback. |
| `d` | Open Connections: devices, people, hosts, and handoffs. |
| `shift+i` | List every agent on every machine. |
| `shift+b` | View new images. |
| `?` | Show the key menu. |

Your configuration can change these keys. Run `vibeke keys` to see active bindings.

## Copy mode, search, and mouse selection

Press `prefix` then `[` to enter **copy mode**. Copy mode moves a cursor over the visible screen and the scrollback of the pane. It uses vi keys by default. Set `mode = "emacs"` under `[keys.copy_mode]` for emacs keys.

| Key | Action |
| --- | --- |
| `h` `j` `k` `l` or the arrow keys | Move the cursor. |
| `w` / `b` | Jump to the next / previous word. |
| `0` / `$` | Start / end of the line. |
| `g` / `G` | Top / bottom of the scrollback. |
| `v` / `V` / `ctrl+v` | Select by character / by line / by block. |
| `y` or Enter | Copy the selection and leave copy mode. |
| `/` / `?` | Search forward / backward. Press Enter to search. |
| `n` / `N` | Next / previous match. |
| `q` or Escape | Leave copy mode. |

A search ignores case while your text is all lower case. It becomes case-sensitive when you type a capital letter.

Press `prefix` then `/` to enter copy mode with the search box already open. The search also reads the older lines that Vibeke saved on disk. If the match is very far back, the search tells you to use `vibeke search`.

You can also select text with the mouse. Drag over text in a pane to select it. A double click selects a word. A triple click selects a line. Vibeke copies the selection when you let go of the button. Set `copy_on_select = false` under `[clipboard]` to keep the selection open in copy mode instead.

Some programs in a pane, such as editors, use the mouse themselves. Hold **Shift** while you drag to select text in these panes. Your host terminal can take Shift for its own selection. In that case, set `mouse_select_in_apps = "always"` under `[clipboard]`.

Pasting does not work in copy mode, navigate mode, or resize mode, because they have no text field. Vibeke shows a notice. Press Escape first. A paste into a search box works.

## Agent state in the sidebar

Each agent row shows a symbol for its state, such as `●` for working or `✓` for done.
A `~` after the symbol means that Vibeke inferred the state from the screen, because the agent did not report it. An inferred state can be wrong. A state that the agent reports through its integration has no `~`.
Set `show_state_source` under `[ui.sidebar]` to `never` to hide the marker.

An agent on a machine that is offline is dimmed and ends in `last known`. Its age stops counting when the connection is lost.

The sidebar keeps its width when an agent changes state. A long label is cut with `…`. On a terminal narrower than 80 columns, the sidebar hides so that the panes keep their width. Press `prefix` then `b` to show it again.

## Themes and colors

Vibeke has these themes: `catppuccin` (dark), `catppuccin-latte` (light), `tokyo-night`, `dracula`, `nord`, `gruvbox` (dark), and `terminal`. The `terminal` theme uses only the colors of your host terminal. It marks the selected row with reverse video, so it reads well on light and dark backgrounds.

By default, Vibeke follows your terminal. It uses `dark_name` on a dark terminal and `light_name` on a light one. Set `name` under `[theme]` to always use one theme. A `name` that you set wins over the automatic switch.

```toml
[theme]
name = "nord"
```

Vibeke warns once when a theme name is unknown, and it falls back to `catppuccin`.

To make your own theme, create the file `themes/<name>.toml` next to your `config.toml`. Name it in `name`. The file holds color tokens. The optional `base` picks a built-in theme to start from.

```toml
# themes/sunset.toml
base = "gruvbox"
accent = "#ff7a59"
```

The tokens are `bg`, `panel`, `fg`, `muted`, `accent`, `red`, `yellow`, `green`, `blue`, `border`, and `selection`. Write colors as `#rrggbb`, or `reset` for the color of your terminal. To change single tokens of any theme, use `[theme.custom]` in `config.toml`.

Set the `NO_COLOR` environment variable to turn off all colors. Vibeke keeps bold, dim, and reverse video. A terminal with only 16 colors, such as the Linux console, gets the nearest of those colors.

## Setup and integrations

First-run setup checks terminal support and offers integrations for detected agents.
It also configures notifications and your theme. Existing Herdr configuration can be imported when detected.

Select integrations with Space. Press **d** to inspect changes, then **i** to install selected integrations.
Claude Code, Codex, pi, and omp start selected when Vibeke finds them. OpenCode and Gemini CLI are marked **experimental** and start unselected.
Confirm the listed files with **y**. Save the configuration at the final step.

The theme step offers **auto**, which follows the light or dark appearance of your terminal, and fixed themes. A fixed theme stays when your terminal changes between light and dark.

Press Escape to skip setup. Vibeke then does not open setup by itself again. Reopen **Setup** from the palette to change these choices.
For troubleshooting, use `vibeke integration status all` and `vibeke doctor`.

## Agent requests

Open **Attention inbox** to review permission requests, questions, and plan reviews.
Select the request, read its details, and answer it. Delivery depends on the agent integration.
See [agents and interactions](concepts/agents.md).

## Images

Press `prefix+shift+b` to view new images. The sidebar also lists recent images. See [previews and browsers](concepts/previews.md#images-in-the-terminal-interface).

## Phone and desktop access

The desktop app has a **Connect to this Mac/computer** flow.
**Connections** (`prefix` then `d`) has four tabs, switched with `tab` and `shift+tab`: **Devices** pairs a phone or browser (also **Pair a phone** in the command palette) and lists and revokes paired devices; **People** lists, creates and revokes colleagues' shared access to a pane or workspace; **Hosts** manages your paired hosts and handoff invitations; **Handoffs** lists incoming handoffs (`prefix+shift+h`).

See [desktop access](desktop.md) and [phone and browser access](mobile.md).
