# 08 — UX, configuration and keybindings

This section covers the TUI client's user-facing surface: layout chrome, navigation, notifications, interaction cards, the full `config.toml` schema, keybinding syntax and defaults, hot reload, and the Herdr importer. Rendering and input mechanics are in [03-terminal-engine-and-tui.md](03-terminal-engine-and-tui.md). The objects shown here (workspaces, groups, agent runs, interactions, notifications) are defined in [02-data-model-and-event-log.md](02-data-model-and-event-log.md).

Proposed task-aware UX: [15](15-task-outcomes-review-and-attention.md) builds on the focused-pane rule with optional task tracking, review packages and a richer attention inbox. The M1 inbox evolves in place on the same `prefix+i` binding; task readiness labels, five-minute view and unified next-attention ordering are staged additions. Current M1 delivery remains independently scoped.

**Milestones** used throughout (see [11](11-milestones.md)): **M0** spikes · **M1** supervision slice · **M2** safe yolo + more harnesses · **M3** remote + preview · **M4** VMs + polish/parity · **M5** compatibility + plugins · **M6** hardening/Windows/1.0 · **post-1.0** deferred unless demanded.

UX principles:
0. **The focused pane belongs to the agent.** Vibeke never draws over, re-renders, or intercepts keys in the agent TUI you are looking at (only the prefix key is Vibeke's). People know Claude's, Codex's and pi's own UIs; those stay exactly as their vendors ship them. Vibeke's structured surfaces — interaction cards, peek, inbox, fleet tiles' timelines, notifications with actions — are for agents you are **not** looking at (other panes, tabs, workspaces, machines; the phone in Phase 2). Answering a card resolves the agent's own dialog through its native channel, and the agent's UI updates itself. Nothing appears over the focused pane unless you explicitly invoked it (palette, goto, peek, `prefix+a`), and even then no bytes reach the agent until you act.
1. **Familiar defaults work on day one.** Default prefix `ctrl+b`, tmux-style default bindings, and an importer for Herdr config.
2. **What needs me is always visible**, but never steals focus.
3. **Structured, not scraped — around the agent, not over it.** Vibeke understands agents through hooks/extensions/RPC so it can tell you *elsewhere* that an agent needs you and let you answer from there. In the focused pane you use the agent's own UI. Vibeke-rendered transcripts (headless runs) are for the phone and automation, not the desktop default: on the desktop every agent you start runs its real TUI.
4. **Show uncertainty.** Inferred states look different from reported ones.
5. **Everything has a command.** Every action is in the command palette, bindable, and available over the CLI/API.

---

## 1. Screen anatomy

```
┌─ sidebar ───────────────┬─ tab bar (top|bottom) ─────────────────────────────────────────────┐
│ ▾ clients               │ 1 api  2 web*  3 ⚑review                                            │
│   ▾ samplehub      ●2   ├─────────────────────────────────┬──────────────────────────────────┤
│     ◆ fix-login  ⚠ ✓    │                                 │                                  │
│       claude  ⚠ approve │          pane w5:p18            │          pane w5:p20             │
│       codex   ● working │          (claude)               │          (codex)                 │
│     ◆ seo-todo   ✓      │                                 │                                  │
│   ▸ dashboard       ○    │                                 │                                  │
│ ▸ personal              │                                 │                                  │
│ ─ pinned ─              ├─────────────────────────────────┴──────────────────────────────────┤
│   backend/claude  ? ask  │ status: devbox ● │ k7 fix-login ⎇ fix/login :4100 │ 2 need you │ 22:41│
└─────────────────────────┴──────────────────────────────────────────────────────────────────────┘
```

Regions: **sidebar** (left or right, collapsible), **tab bar** (top or bottom), **pane area** (tiled layout + floating panes), optional **status bar** (top or bottom, independent of the tab bar), **user-invoked popups** (palette, switcher, peek, interaction cards, toasts — toasts sit outside the focused pane's frame where possible and never take keyboard focus).

## 2. Sidebar

### 2.1 Tree
- Levels: **Machine** (shown only when more than one machine is connected) → **Group** (optional, nestable) → **Workspace** → **Agent rows** (one per live agent run in the workspace) → optional **plain pane rows** (`ui.sidebar.show_shell_panes = false` by default).
- Tasks (05) render as workspaces with a `◆` marker plus branch name. A task workspace sits under its source repo's workspace when `ui.sidebar.nest_tasks = true` (default).
- Collapsing a node aggregates its children: `●2` = two working, a red badge = needs you. Aggregates always bubble up the **most urgent** child state (precedence: `needs_approval` > `needs_answer` > `error` > `rate_limited` > `done` > `working` > `idle` > `unknown`).
- Keyboard: navigate mode (§6) or `prefix+w` picker. Mouse: click to focus, drag to reorder or move into a group, right-click opens a context menu.
- **Needs-you section on top** (learned from Claude Code agent view): when any run has an open interaction or is `error`/`rate_limited`, a `─ needs you ─` section lists those rows first, oldest first, across all workspaces and machines; the rows still also appear in their workspace. `ui.sidebar.attention_section = true` (default).
- *As built (M4 TUI, `vk-tui::groups`):* per machine, groups come first (by `order`, children nested, two spaces per level), then ungrouped workspaces. A group row is `▾ name` (`▸` when collapsed) with aggregate badges over every member workspace including child groups: `⚠n` runs with an open approval/question, `✗n` error/rate-limited, `●n` working, `✓n` done-unseen; a collapsed group with no agents shows `(n)` workspaces. Group rows are selectable in navigate mode: `enter`/`space` toggle, `l`/`→` expand, `h`/`←` collapse, `r` rename; other pane keys are ignored on a group row. On any row `m` opens a "move to group" picker (`(no group)`, every group, `+ new group…`) and `G` creates a group. Mouse: click a group row to toggle; drag a workspace or agent row onto a group row to move that workspace in, or onto an ungrouped workspace row to take it out (same machine only). Collapse is drawn at once and sent as `group.collapse {group, collapsed}`. Palette: `group_new` (creates a group and moves the focused workspace into it), `group_move`, `group_rename`, `group_collapse`. Not built: reordering workspaces by drag, the right-click menu.
- *As built (v1 TUI, `vk-tui::groups`):* **group drag reorder.** A press on a group row no longer toggles at once: releasing on the same row is the click (toggle); dragging onto another group row of the same machine moves the dragged group next to it among that row's siblings — after it when dragged downwards, before it when dragged upwards — with `group.move {group, parent, index}` (the parent is the target's, so dropping on a child group re-parents to that level). The dragged row shows ` ⇅ moving` while the pointer is over the sidebar; a drop on the group itself or on a group inside it is refused locally with a toast ("a group can't move into itself"), as the server would refuse the cycle.
- *As built (v1 TUI, `vk-tui::taskbadge`):* **task badges.** A task workspace row shows its pull request from the server's **cached** `task.pr` result — ` #123 ✓`, `✗ checks`, `…` (pending), `draft`, `merged`, `closed` — green/red/yellow by checks, accent for merged, muted for closed. The TUI never runs `gh` and never asks for a refresh: it reads the cache through `task.get` (which never runs `gh`), once when a task workspace with a checkout appears and then every 60 s (the server's cache lifetime; a `tasks.pr` deadline), only for tasks with a workspace and a checkout that aren't archived, finished or missing. Nothing cached = no badge (the cache fills when something asks: `vibeke task pr`, the server's own lookups); a server without `task.get` stops being asked. **Missing checkouts:** a task reconcile marked `missing` (05 §4) shows ` ⊘ missing` on its row and offers **recreate** (`task.recreate {task}`, a new checkout from the task's branch) and **forget** (`task.forget {task}` after a confirmation; the record goes, nothing on disk is touched): navigate-mode `R` / `F` on the task's row, palette entries per missing task (`Recreate missing task #k7 … — new checkout from its branch`, `Forget missing task #k7 … — nothing on disk is touched`), and the palette actions `task_recreate` / `task_forget` for the focused workspace's task (else the only missing one, else the palette filtered to `missing task`). A server without those methods (`method_not_found`) gets a toast saying so; nothing is retried on its own.
- *As built (Batch 2B, `vk-tui::sidebar`):* with `nest_tasks = true` a task workspace whose task's `repo_root` equals another (non-task) workspace's root is drawn under that workspace, one level deeper, and not at the top level; with `false` it stays at the top level.

### 2.2 Agent row anatomy
`<harness icon/name> <name?> <state glyph> <short label> <age?>`

| State | Glyph | Colour token | Label |
|---|---|---|---|
| starting | `◌` | `muted` | starting |
| working | `●` (pulsing at ≤ 2 Hz when `ui.animate`) | `accent` | working · 3m |
| needs_approval | `⚠` | `red` | approve: `Bash pnpm test` (truncated) |
| needs_answer | `?` | `yellow` | ask: first question |
| done (unseen) | `✓` bold | `green` | done |
| idle (seen) | `○` | `muted` | idle |
| error | `✗` | `red` | error: rate limit / API |
| rate_limited | `⏸` | `yellow` | limited until 23:10 |
| exited | `⊘` | `muted` | exited (code) |
| unknown | `·` | `muted` | — |

*As built (Batch 2B):* with `ui.animate = true` the working `●` alternates bold/dim every 500 ms (2 Hz). The repaint rides on the working age label's existing deadline class (`ages`), so an idle client still arms nothing.

`done` is **not an execution state**: it is an attention marker on an `idle` run (02: execution state vs per-client attention state). It renders as `✓` bold until the run has been seen in any attached client, then as `○`. Automation (`agent wait`) never depends on whether a human looked.

**Isolation glyph** (13): a short column before the state glyph shows where the run executes: blank = host, `sb` = OS sandbox, `ct` = container, `vm` = VM, `@devbox` prefix for remote machines. Glyphs are themeable (`[ui.sidebar.isolation_glyphs]`).

**Yolo badge**: runs detected as yolo (approval bypass flags such as `codex -a never -s danger-full-access`, `claude --dangerously-skip-permissions`, or a harness with no approval system like pi) get a `YOLO` badge — neutral colour inside sb/ct/vm, **red `YOLO·HOST`** when executing on the host. The badge is informational only; Vibeke never blocks commands the user typed (13 §3.1).

**Source/confidence indicator** (principle 4): the state glyph is drawn normally for `source = adapter | self_report`. For `screen` or `process`, or when `confidence < 0.8`, it is drawn **hollow/dim with a trailing `~`** (`⚠~`), and the tooltip or `info` shows `inferred from screen (0.72)`. `ui.sidebar.show_state_source = "inferred-only" | "always" | "never"`.

### 2.3 Unread, marked-unread, pinned
- **Unread**: a pane gets `unread` when it produced output or changed state while not visible in any client. Shown as bold row text. Cleared when focused.
- **Mark unread**: `prefix+u` or context menu toggles `marked_unread` on the current agent/pane. It stays until explicitly cleared or focused again (`ui.marked_unread_clears_on_focus = false` keeps it until toggled).
- **Pinned**: `prefix+alt+p` pins the pane. Pinned rows also appear in a `─ pinned ─` section at the bottom of the sidebar, in pin order.
- `done` vs `idle` works like this: idle work finished while the user wasn't looking is `done` until seen. "Seen" means focused in any attached TUI client (CLI reads don't count; Phase 2 mobile clients will).

### 2.4 Placement and sizing
- `ui.sidebar.position = "left" | "right"`.
- `ui.sidebar.width` (default 28), `min_width` 18, `max_width` 48, auto-fit to names when `ui.sidebar.auto_width = true`. Drag the border to resize; the width is saved per client.
- `ui.sidebar.collapsed = false`; `prefix+b` toggles. When collapsed, a 2-column rail shows only urgency badges per workspace.
- Row tokens are configurable (including `hide = true`) via `[[ui.sidebar.token]]` rules: match by harness, state or regex, then rename, recolour or hide.
- *As built (M4 TUI, `vk-tui::chrome`):* `position = "right"` draws the sidebar in the last `width` columns with its `│` border on its left; the tab bar, status bar and panes take the columns to its left. All placement goes through `chrome` (sidebar column range, border column, main-area span, tab row), so hit tests (sidebar rows, groups drag, preview rows, status-bar clicks) follow it. Palette `sidebar_side` flips it for this client without touching the config. Not built: dragging the border to resize, `auto_width`, the collapsed rail.
- *As built (Batch 2B, `vk-tui::sidebar`):* **width** — `auto_width` grows the sidebar to the widest row but never below `width`, clamped to `min_width..=max_width`; dragging the border sets a manual width (clamped the same way) that wins over `auto_width`, saved per client in `<state>/<session>/sidebar-<client>.json` and restored on attach (palette `sidebar_width_reset` forgets it). **Rail** — while collapsed, a 2-column rail (glyph + border, on the sidebar's side) shows one urgency glyph per workspace on every machine (`⚠ ? ✗ ⏸ ✓ ●`, `·` for nothing); the focused workspace is highlighted, clicking a glyph focuses that workspace, clicking `≡` on the top row opens the sidebar. **Token rules** — `[[ui.sidebar.token]]` applies to agent rows: `harness`, `state` (`working idle done needs_approval needs_answer error rate_limited starting exited unknown`) and `regex` (against `name label`) must all hold when given; the first matching rule wins; `hide` drops the row (also from the needs-you section), `label` replaces the name, `color` (`#rrggbb`, an ANSI index or `red yellow green blue accent muted fg`) recolours it.

## 3. Tab bar

- Position `ui.tabs.position = "top" | "bottom" | "hidden"`.
- Each tab shows `number` plus title: `3 review`. `ui.tabs.show_numbers = true`. Numbers are per workspace, assigned at creation, and renumbered only on explicit `tab renumber`.
- Title source order: custom title → `auto_title` (the focused pane's agent name or harness, else the process name, else OSC title) → `shell`.
- The tab shows the most urgent agent state among its panes as a glyph prefix (`⚑`, `?`, `✓`).
- *As built (lane 1B, 03 §8):* an OSC 9;4 progress report from any of the tab's panes appends a 4-cell bar to the label (`3 build ▰▰▱▱`; `⋯` for indeterminate; an error state wins over the furthest-along pane). The workspace's sidebar row and a shell pane's row show the same bar (red for error, yellow for paused). A failed command's exit code (`OSC 133 ; D ; n`, n ≠ 0) shows as ` ✗ exit n ` for 5 s: in the corner of an unfocused pane, in the tab bar's right cluster for the focused pane (the focused pane is the agent's, §0).
- Overflow: scroll arrows plus the `prefix+g` goto. Mouse: click to switch, middle-click to close (confirm if processes are running), drag to reorder.
- *As built (M4 TUI, `vk-tui::chrome`):* `"bottom"` puts the tab row (tabs, preview chips, mode/toast/notice cluster on the right) on the last screen row and the pane area starts at row 0; a bottom status bar then sits directly above it, a top one on row 0. `"hidden"` draws no tab row; the right-hand cluster (mode, toasts, clipboard/pending notices) is drawn over the top-right of the pane area only while it has something to show. Clicks on the tab row work wherever it is. Palette `tab_bar_position` cycles top → bottom → hidden for this client. Not built: overflow arrows, middle-click close, drag to reorder.
- *As built (Batch 2B, `vk-tui::tabbar`):* when the tabs don't fit, `‹`/`›` mark hidden tabs and a click scrolls by one tab; the window follows the focused tab, and a manual scroll holds until the focus moves to another tab. Middle click closes a tab (a confirm when one of its panes runs a non-shell process). Pressing a tab focuses it; dragging it over the others shows a `▏` drop marker and the release sends `tab.move {tab, delta}` (numbers stay). `show_numbers = false` drops the number.
- *As built (v1 TUI, `vk-tui::tabbar`):* **`tab renumber`** is the palette action `tab_renumber` (unbound by default): it asks the server to renumber the focused workspace's tabs 1..n in their current order with `tab.renumber {workspace}` and toasts `tabs renumbered 1..n`. Numbers change nowhere else (drag reorder keeps them). The server method is not in the API yet (07 §2.6 lists `tab.move` only); until it is, the action answers `method_not_found` with "this server can't renumber tabs yet" and changes nothing.

## 4. Status bar (optional)

`ui.status_bar.enabled = false` by default. When enabled, `position = "top" | "bottom"`, with segments on the left, centre and right:

```toml
[ui.status_bar]
enabled  = true
position = "bottom"
left     = ["machine", "task", "branch", "ports"]
center   = ["attention"]           # "2 need you" — click opens the card for the oldest unfocused one
right    = ["agents_summary", "clock"]
```

Built-in segments: `machine`, `session`, `workspace`, `task`, `branch`, `ports` (task port range and live previews), `attention`, `agents_summary` (`●3 ✓1 ⚠1`), `cpu`, `clock`, `prefix_indicator`, `mode` (normal/navigate/copy/resize), `sync_input`.

*Implementation (M4, server side):* `status.segments {pane?, client?}` (07 §2.1) returns the data for every built-in segment from a client's focus: machine, session, workspace, task, branch, ports with live previews, attention (count plus oldest unfocused interaction), agents_summary (working/done/needs_input/error), cpu load, clock and sync_input. `mode` and `prefix_indicator` are client state.

*As built (M4 TUI, `vk-tui::statusbar`):* `ui.status_bar.enabled` turns it on; `position = "top"` puts it on the row under the tab bar, `"bottom"` on the last row (the pane area shrinks by one row either way). Lists are separated by ` │ `; the left list starts at the pane area's left edge, the centre list is centred and the right list right-aligned (on a narrow bar later lists overwrite earlier ones). Data comes from `status.segments` on the focused machine: requested at most once a second after a model change, and every 10 s otherwise (for `cpu`); one request in flight at most. Until the first reply, or when the server has no `status.segments`, the model-derived segments (machine, session, workspace, task, branch, attention, agents_summary) are computed locally. `clock` (local `HH:MM`, redrawn when the minute turns), `mode` and `prefix_indicator` are drawn locally. Clicking `attention` opens the oldest unfocused card (`next_attention`). Palette `status_bar_toggle` flips it for this client without touching the config. Empty segments are skipped; plugin segments (M5) render nothing yet.

**Plugin segments** (M5): plugins contribute `status.segment` with `{id, interval_ms | event_driven, render → spans}` (07). They appear as `plugin:<id>/<segment>` in the lists above. A segment that renders slower than 50 ms is dropped with a warning.

## 5. Panes, floating panes, popups, zoom, resize

- **Splits**: `prefix+v` (vertical, side by side), `prefix+minus` (horizontal). `pane.split` via API supports `--size 30%`.
- **Zoom**: `prefix+z` toggles the focused pane to fill the tab. A `Z` marker appears in the tab.
- **Resize mode**: `prefix+r` then `h j k l` / arrows (shift = ×5), `=` equalizes, `esc`/`enter` exits. Mouse: drag borders.
- **Floating panes** (M4): `prefix+f` creates a floating pane (default 70%×70%, centred); `prefix+shift+f` toggles visibility of all floats in the tab. Floats can be moved/resized with the mouse or in resize mode (`m` toggles move). A tiled pane can be floated and back (`pane float`/`pane embed`).
  - *Implementation (M4, server side):* `Tab.floating: [{pane, x, y, w, h, z}]` (percent of the pane area) and `Tab.floats_hidden`; `pane.float`, `pane.embed`, `tab.floats` (07 §2.6). Floats survive layout export/apply; closing a tab's last tiled pane closes its floats. Open TUI work: draw floats over the tiling (z order, border, focus ring), skip them when `floats_hidden`, `prefix+f`/`prefix+shift+f`, mouse move/resize and resize-mode `m`, and include floats in `ViewHint` so their PTYs get real sizes (they stay 80×24 until then).
  - *As built (M4 TUI, `vk-tui::floats`):* floats draw after the tiling in `z` order: the rect is cleared (tiled cells and browser-tile placeholders under it are overwritten, so images clip), then a rounded frame (`╭╮╰`, `◢` resize handle) in the accent colour when focused, with the agent name or pane title on the top row. Geometry: percent of the pane area → cells, at least 6×3, kept inside the area. The content rect (frame excluded) is what `App::pane_rects` returns — floats first, topmost first, then the tiling — so hit tests, `ViewHint` sizes, browser panes inside floats and focus cycling all see floats; hidden floats (`floats_hidden`) and floats of a zoomed tab are skipped everywhere. Mouse: pressing a float's title row focuses and raises it and starts a move; the bottom-right corner starts a resize; the drag is drawn locally and sent once on release as `pane.float {pane, rect}` (a model update mid-drag keeps the dragged rect); a press inside a lower float's content raises it (`pane.float {pane}`) and goes to the pane as usual. Keys: `prefix+f` new float (`pane.float {tab, focus}`), `prefix+shift+f` hide/show (`tab.floats`, focus moves to the tiling when the focused float hides); in resize mode on a float `h j k l` resize by 2% (shift: 10%), `m` toggles moving (title shows `· move`), `=` re-centres at 70%×70%. Palette: `float_pane` (float the focused tiled pane ⇄ embed the focused float) and `embed_pane`. A tiled pane's cursor under a float is hidden.
- **Popups** (`type = "popup"`): session-modal terminals that don't change the layout and close when the command exits (used by `[[keys.command]]`, edit-scrollback, plugin actions). Width and height accept cells or `%`.
  - *As built for Herdr plugin panes (M5 slice 3, `vk-tui::plugins`):* `plugin.pane.open {placement: popup|overlay}` runs the command in a floating pane tagged as a plugin surface (07 §2.16); ordinary float handling (frames, drag, resize mode, `prefix+shift+f`) skips them. A **popup** is a centred window sized by the manifest/request `width`/`height` (cells or `%`, default 80%×80%, clamped to the pane area) with an accent frame, `title · plugin` on top and `prefix+x closes` at the bottom. It is modal for the whole session: it takes the focus back whenever it lives (also from another tab), every key goes to its terminal (direct bindings are suspended; `esc` belongs to the command while it runs), `prefix prefix` passes the prefix through, `prefix+x`/`prefix+q`/`prefix+esc` (or the `close_pane` binding) dismiss it, other prefix chords are refused with a hint, and clicks outside it are ignored. An **overlay** is a full-area layer over the current tab (not a zoom): a one-row header `⧉ title · plugin` with `prefix+x closes`, the terminal below; nothing under it is drawn, hit, focused by cycling or reported in `ViewHint`; prefix actions keep working and `close_pane` on it dismisses it. Both close when their command exits (`esc closes` is shown and dismisses at once); dismissal goes through `plugin.surface.close`, which returns the focus to the pane that had it before. Plugin titles are sanitized (09 §6). Not built: drag/resize of popups, dimming the background, `[[keys.command]] type = "popup"` for user commands (still opens a tab).
  - *As built (Batch 2B, `vk-tui::popup_pane`, server `user_popup.rs`):* `[[keys.command]] type = "popup"` sends `pane.float {popup: {width, height}, command, title, cwd, rect}`; for a full-scope client the server tags the float like a plugin popup (`plugin-surface:popup:<w>:<h>:vibeke/popup`; pane-scoped callers get a plain float), so the popup machinery above applies (modal, `prefix+x`, closes when the command exits) and the focus returns to the pane that had it when the popup is gone. `type = "float"` opens an ordinary floating pane; `cwd = "pane" | "workspace" | path` is honoured. Every popup (plugin ones too) can be moved by dragging its top border and resized from its bottom-right corner (a per-client offset for the popup's life; view hints follow), and the pane area under an open popup is dimmed. `when = "agent:<harness>"` is enforced: a direct binding whose condition doesn't hold passes the key to the pane, a prefix binding says why nothing ran.
- **Synchronized input** (post-1.0): `prefix+shift+s` toggles sync for the current tab. Input to the focused pane is mirrored to every pane in the tab that is in the sync set (default all; `prefix+alt+s` toggles a single pane's membership). A bright `SYNC` badge shows in the tab bar and the status bar. Paste is mirrored too. Agent panes are **excluded by default** (`ui.sync_input.include_agents = false`) to avoid prompting N agents by accident.
  - *As built (Batch 2B, `vk-tui::sync_input`):* the fan-out is client-side (one `Key`/`Paste` frame per pane, each with its own input id), only from a focused pane that is itself in the set; browser panes, popups and exited panes never take part. `prefix+alt+s` (`sync_input_pane`) on an agent pane adds it (agents excluded by default), on a terminal pane takes it out. Indicators: ` SYNC n ` (n receiving panes) in reverse video on the right of the tab bar, `⇉` on the tab label, and the status bar's `sync_input` segment (client state first, then the server's). Quick exit: `prefix+shift+s` again or the palette's `sync_input_off`. Sync is per tab and per client; a closed tab is forgotten.
  - *As built (batch 2 review, 2026-10-06):* agent exclusion is enforced by the server, not only by the client's model. To a server that lists the `sync_input` render feature, mirrored keys and pastes go as `ClientFrame::SyncInput` with an `include_agent` flag (the pane was added with `prefix+alt+s`, or `ui.sync_input.include_agents = true`); the server drops a mirrored input to a pane with a live agent run unless that flag is set, so an agent started in a synced shell pane by another client, before this client's model caught up, never receives mirrored prompts or approval keys (07 §3). Older servers get plain frames. Tests: `sync_input_tests::mirrored_frames_carry_the_inclusion_flag_for_server_enforcement`, `run::auth_tests::mirrored_input_never_reaches_a_live_agent_pane_unless_included`.
- **Focus follows mouse**: `ui.focus_follows_mouse = false`. When enabled, hovering a pane focuses it after `ui.focus_follows_mouse_delay_ms = 120`; it never applies while a popup or card is open.
  - *As built (Batch 2B, `vk-tui::mouse_focus`):* pointer motion over another pane arms a deadline of `focus_follows_mouse_delay_ms`; the pane is focused when it passes, unless the pointer left it or the client is no longer in normal mode (any popup, card, prompt, copy/navigate/resize mode, or a plugin/user popup cancels it).
- **Scroll requests** (07 §2.6 `pane.scroll`): scrolling is client-side, so the server publishes `pane.scroll_requested {offset, total, client}` and attached clients move their own view.
  - *As built (v1 TUI, `vk-tui::scroll_req`):* the TUI subscribes to `pane.scroll_requested`; a request naming another client is ignored. For the pane focused in this client the view moves at once: offset 0 leaves copy mode (the live screen), any other offset enters copy mode (or moves the open one) with the top of the view `offset` rows above the live screen's first row, loading in-memory history first when the offset reaches past what is loaded (clamped to what exists). A request for a pane that isn't focused here, or while a popup, prompt or another mode owns the keyboard, is kept and applied when that pane is next focused in normal mode; a later request replaces it, offset 0 drops it, and requests for closed panes are forgotten. Focus never moves and nothing is sent to the pane.
- **Close**: `prefix+x` closes the pane after confirmation when a non-shell foreground process is running (configurable `ui.confirm_close = "running" | "always" | "never"`).

## 6. Navigation

### 6.1 Navigate mode
`prefix+w` opens navigate mode: the sidebar gets keyboard focus. Movement: `up/down` move between workspaces and agents, `h j k l` move between panes, `enter` focuses, `1..9` jump, `esc` exits. Plus: `/` filter, `space` peek (§6.4), `a` answer (opens the interaction card for the selected agent, if unfocused), `u` mark unread, `p` pin, `r` rename, `x` close (confirm), `n` new workspace, `t` new task (05).

*As built (Batch 2B, `vk-tui::navkeys`):* `/` types a filter (shown as `NAV /text` in the tab bar) that keeps only selectable rows containing it (case-insensitive); `enter` stops typing and keeps the filter for moving, `esc` clears it (a second `esc` leaves); the filter ends with navigate mode. `p` pins the selected pane (`pane.pin`), `t` focuses the selected row's pane and asks for a task title (the task is created in that pane's repo). Unchanged deviation: `j`/`k` move the selection; moving between panes with `h`/`l` from navigate mode is not built.

### 6.2 Goto / fuzzy switcher
`prefix+g`: one fuzzy list over workspaces, tabs, panes, agents (by name, harness, state), tasks, previews and machines. Typing filters; prefix tokens narrow the kind: `@agent`, `#task`, `:tab`, `>command` (switches to the palette), `!state` (`!approve` lists everything awaiting approval). Results are ranked by match score, then recency, then urgency. `enter` jumps; `ctrl+enter` opens in a new split (M4, built below).

*As built (research R5):* `vk-tui::nav`. Candidates: workspaces (`~`), tabs (`:`), panes/agents (`@` for panes with a run) and tasks (`#`; untracked tasks only when asked for). Matching is a real fuzzy matcher (fzf-style subsequence with word-boundary/camelCase/first-char bonuses, consecutive-run bonus, gap penalties; every whitespace token must match) over the shown label **plus hidden fields**: workspace root/repo path and git branch, tab/pane handles, pane cwd, the agent's harness, model, run handle and **native session id** (`harness_session_id`), task repo/branch/worktree. Matched letters are highlighted (bold+underline) in the label. Ranking: score, then this client's **recent targets** (per-client history, §6.7), then urgency (open interaction, question, done), then natural order; with an empty query recent targets come first, then urgent ones. `!state` filters by run state (`!approve` = open interactions), kind prefixes narrow, `>` as the first key switches to the palette.

*As built (v1 TUI, previews and machines):* goto also lists every machine's previews (not gone) as `%` entries — `v4 :5173/app vite`, matched also by URL and the preview's pane — and, with more than one machine, each machine as a `^` entry (`devbox · connected · 3 agent(s)`, urgency = its most urgent run). `%`/`^` narrow like the other kind prefixes; `!state` never matches machine entries. `enter` on a preview opens it as configured for its workspace (`[preview] mode`/`pane_split`, a trusted repo's `[preview]` included, §11.1: a browser pane next to its pane by default, the window fallback without graphics), `alt+enter` opens it in the profile browser window; `enter` on a machine switches to it (its focused pane, else its first tab), an offline machine toasts instead.

*As built (M4, secondary action):* `enter` jumps. `ctrl+enter` and `alt+enter` are the same key (the secondary action): on a workspace, tab, pane/agent or task entry they open a **new split next to the focused pane** (`pane.split`, direction right, focused) whose shell starts in the entry's directory (pane cwd; a tab's focused pane cwd; workspace root; task checkout). The split opens on the current machine only; picking an entry of another machine toasts instead. When the query is a directory path (`/abs/path` or `~/path`; `~` alone stays the workspace prefix) the same key creates a **new workspace** there (`workspace.create`, focused), and the empty-result row says so. The popup title reads `... · alt+enter split`. `ctrl+enter` is only distinguishable from `enter` when the host speaks the kitty keyboard protocol or modifyOtherKeys level 2 (both are enabled at startup, 03 §7.1); on other terminals it arrives as plain `enter` and just jumps, so **`alt+enter` is the portable binding** and works everywhere. The palette treats both as `enter`. Not built: a second "client split view" mirroring an existing pane; the Question card's `ctrl+enter` submit (see the cards section) does not exist in the TUI yet.

### 6.3 Command palette
`prefix+p` is taken by `previous_tab`, so the palette is **`prefix+:`** and also `ctrl+shift+p` as a direct binding when the host reports it unambiguously (kitty keyboard). It lists every action (built-in, `[[keys.command]]`, plugin actions) with its current binding. Actions take arguments through inline prompts (for example `split: size?`). It remembers the last 20 used.

*As built (research R5):* `Popup::Palette` (`prefix+:`; the `:` text prompt it replaced is gone). Entries: every `DEFAULT_KEYMAP` action with a one-line description and its effective binding, the browser-pane context actions (binding shown as `… (browser pane)`), palette-only TUI commands (`track_work`, `task_details`, `pending_operations`, `open_preview`, `preview_window`, `preview_proxy`, `preview_mirror`, `preview_unmirror`, `browser_stop`, `browser_watch`, and since §6.7 was built `screenshots`, `screenshot_pane`, `desk`, `drafts`, `notes`, `assist_briefing`, `assist_pane_title`), `[[keys.command]]` entries (`Command: <title>`), and **live entries** `Watch agent browser b3 — <url>` per agent browser session on each connected machine (listed with `browser.list` when the palette opens; refreshed by pushed `browser.*` events). Fuzzy-matched against "description  action_name" with highlights; recently used entries (last 20, per client, persisted) come first with an empty filter and get a bonus otherwise. `↑/↓`, `tab`, `ctrl+n/p` move, `ctrl+u` clears, `enter` runs, `esc` closes. *Inline arguments (Batch 2B, `vk-tui::navkeys`):* palette entries for `split_vertical`/`split_horizontal` ask `size?` (`30%`, `0.3`; empty = half; sent as `pane.split` `ratio`, the new pane's share), `switch_tab` and `switch_workspace` a number, `focus_agent` an agent number (all machines, model order), `new_tab` a command (empty = shell); their key bindings run without asking. `ctrl+shift+p` opens the palette directly (added unless the user rebinds the chord; legacy hosts send plain `ctrl+p`, which never matches). *Plugin actions (M5 slice 3):* each connected machine's `plugin.action.list` (fetched after connecting and whenever the palette opens) adds `Plugin: <title> (<plugin id>)` entries (`[machine]` with several machines), with the binding from a `[[keys.command]] type = "plugin_action"` entry; enter runs `plugin.action.run {plugin, action, pane, source: palette}` in the focused context and toasts the start or the error. Actions of untrusted, stale-trust or disabled plugins are listed dimmed and disabled with the fix (`untrusted — trust with vibeke plugin trust <id> --legacy`, `disabled — enable with vibeke plugin enable <id>`); enter only shows that hint. *Plugin completion:* entries are filtered by the action's `contexts` for the focused machine (`global` always; `workspace`/`tab`/`pane` need that focus; `selection` needs a copy-mode selection, so it is never offered from the palette); the binding shown also covers a plugin manifest's installed default.

### 6.4 Peek and reply (without attaching)
Learned from Claude Code agent view. In the sidebar (or goto list), `space` on an agent row opens a floating peek of that run without changing focus:
- Header: harness, name, task/branch, isolation, state with source, time in state.
- Body: for structured runs, the last assistant message and last tool calls from the transcript (rendered markdown, not terminal cells); for screen-only runs, the last 30 terminal lines.
- Open interaction, if any, as an inline card with the card keys (`y`/`n`/…) (§8).
- **Reply box**: start typing to compose a follow-up; `enter` sends via `agent.prompt` (native for structured runs — pi RPC `follow_up`/`steer`, Codex `turn/start`/`turn/steer`, Claude via typed input with bracketed paste), `alt+enter` sends as steer when the run is working and the harness supports it.
- `enter` on an empty reply focuses the pane; `esc` closes. Peek never marks the run seen unless a reply is sent.
- *As built (06 B7):* when the agent has an agent-browser session the peek shows `◉ browsing b3 · <url>` and `w` opens a read-only watch pane on it (`browser.watch {agent_pane}`); `prefix+t` in that pane takes over.

### 6.5 Workspace and agent cycling
`previous_workspace`, `next_workspace`, `previous_agent`, `next_agent`, `focus_agent` (indexed) and `next_attention` (**default `prefix+a`**: opens the card for the oldest open interaction on an unfocused agent, else focuses the oldest `done`; `prefix+A` focuses that agent's pane instead).

*As built (v1 TUI, `vk-tui::agent_list`, 06 A5):* **cross-machine agent list**, action `agent_list`, default **`prefix+alt+a`** (`prefix+a` is `next_attention` and `prefix+shift+a` `next_attention_focus`; the "all agents" list takes the same letter with `alt`, like `pin_pane`/`sync_input_pane`). A user-invoked list popup of every live agent run on every connected machine (token-rule-hidden runs left out), most urgent first: open approvals/questions (oldest open interaction first), errors, rate limits, done-unseen, working, idle, the rest; ties by time in that state (longest first). Rows: `[machine · ]icon name · workspace` with the state glyph and label and the age on the right; the title counts agents and machines. Typing filters (fuzzy over machine, name, harness, workspace, state, branch); `↑/↓`, `tab`, `ctrl+n/p` move; `enter` focuses the agent's pane on its machine (an offline machine toasts); `alt+enter` opens the card for its open interaction (through the `ui.interaction_overlay` gate, §8), else focuses; `esc` closes. Nothing is sent to an agent.

### 6.6 Fleet grid and inbox views
- **Fleet grid** (`prefix+F`, M4; a view, not a layout change): one tile per agent run. **Tiles default to a live miniature of the real terminal** (the agent's own UI, scaled by cropping to the last rows); `t` toggles a tile (or `T` all tiles) to Vibeke's structured timeline (tool calls, diff size, state, preview). `ui.fleet.tile_view = "terminal" | "timeline"`. `enter` focuses the real pane.
  - *As built (Batch 2B, `vk-tui::fleet`):* bound to **`prefix+shift+m`** (`fleet`) because `prefix+shift+f` is `toggle_floats`. A full pane-area view: tiles in a grid (40×10 cells, scrolled to keep the selection visible), one per run on every connected machine, titled `icon name · [machine/]workspace glyph`. Terminal tiles show the last non-empty rows of `pane.read {source: visible}` from the run's machine, refreshed once a second only while the grid is open (one read per pane in flight); timeline tiles show the state with age and source, last tool, branch, turns and the last message. `h j k l` move, `t`/`T` flip one/all tiles, `enter` focuses the real pane, `esc` closes. Nothing is sent to an agent.
- **Inbox view** (`prefix+i`, M1): a full-screen list of open interactions on unfocused agents, ranked by urgency (risk, wait, blocking), with the card for the selected item and a peek of the agent. It replaces the pane area while open (agents keep running; nothing is drawn over a focused pane because there is none while the view is open). `esc` returns.
  - *Evolved in place for spec 15 T3 (implemented):* the same `prefix+i` view consumes `attention.list` from every connected machine and merges it (coverage lines for offline machines or incomplete coverage; offline items shown as last observed with actions disabled). Items: interactions, review available / check failed / send unknown, suspended bindings, and a "Finished turns" footer. Each row has a deterministic explanation ("Waiting 12m · blocks this run"). Keys: `j/k`, `enter` (card or task details), `y/n/1-9` quick answers, `o` open pane (the only focus change), `s` snooze (15m / 1h / tomorrow 9:00 / custom), `e` effort, `f` five-minute view (urgent items always shown, omitted count visible). Selection is stable by item key while the list reorders; newly urgent items get a ● marker instead of stealing selection. Falls back to the M1 ordering when the server lacks `attention.list`. `prefix+a` follows the ranking.
  - **Task details** (explicitly opened full pane-area view, never automatic): from the inbox, goto (`#k1`), agent peek `t`, or `:task_details`. Shows ownership/lifecycle, review label, intent with the verbatim source request, "Agent has not been told about revision N", bindings (`c` Continue task / `n` Track new work), messages with delivery state, requirements, observed commands (agent prose shown as *claim*), checks; actions `e` edit intent, `w` message agent (prepare → exact text + recipient → explicit send; refused sends offer Open pane), `v` run check (first run asks "Authorize for this revision"), `m` mark reviewed (reason per unsupported required criterion), `o` open pane.
  - **Track this work**: agent peek `t` or `:track_work` → form with the selected request verbatim (↑/↓ other turns), editable title, optional criteria, stop-at starting at *Not specified*, objective; `ctrl+s` tracks. Unverified runs show "Link run first". Sidebar agent rows show the review label as a separate small marker (`◆review`) beside `✓ done`.
  - Pending task mutations persist in `<state>/<session>/client-pending.json` before dispatch and are reconciled via `task.operation.get` on reconnect; unknown outcomes are never resubmitted automatically (`:pending_operations`).

### 6.7 Session desk and drafts composer (research R2/R3)

The server/CLI side is 07 §2.14a (`desk.*`, `draft.*`, `notes.*`). The design below was the plan; the TUI as built (2026-10-06) is described in the *As built* block after it. No new default keybindings: entries live under the palette, the agent peek and task details.

- **Session desk** (`:desk`, goto prefix `?` for conversations): a full pane-area view like the inbox. A query line (`desk.search`), filter chips for repository (default: the current workspace's repo), harness and date (`7d`, `30d`, custom), and result rows `harness · repo · session · turn n · age · snippet` with a status marker (`● live`, `↻ resumable`, `· none`). `enter` opens the result detail (`desk.open` with `turn`: the turn's items rendered like the peek body). Actions, labelled exactly: `o` **Focus live pane** (live only; `desk.open focus=true`), `r` **Resume native session** (shows the exact command and asks for a target: a new tab in the session's workspace or a chosen free pane), `c` **Start new agent with context** (opens the `desk.context` package in an editor buffer with selectable turns; saving creates a draft; optional "start harness" toggle; never sends). A sessions tab lists `desk.sessions`. A footer shows coverage from `desk.status` (sources, opted-in roots, exclusions, retention, pending bytes) and `F` forget (confirm, `desk.forget`).
- **Drafts composer** (`:drafts`, agent peek `d`): a side panel per workspace (and per task in task details) listing drafts in order with attachment counts and the last send state (`sending`, `✓ delivered`, `? unknown`, `✗ failed`). Keys: `n` new, `e` edit (multi-line editor; `ctrl+f` attach a file, `ctrl+s` attach the latest screenshot/clipboard image via `blob.put`), `J/K` reorder (`draft.reorder`), `space` select, `m` combine selected, `x` delete (confirm), `s` send → target picker defaulting to the run in the focused pane, showing `draft.check` (`send_path: prompt_input`, or "Open pane to send" with the reason, plus "steer: not supported") and an "include notes" toggle; refused sends keep the draft and offer `o` open pane. A `? unknown` draft offers `R` reconcile (`draft.reconcile`) before a warned retry. Pending sends persist their idempotency key like task mutations (`client-pending.json`).
- **Notes** (`:notes`, a tab in the drafts panel): one plain-text document per workspace (`notes.get/set`, `expected_rev` conflict prompt), labelled "Never sent unless you include it".
- The agent peek reply box (§6.4) gains "Save as draft" (`ctrl+d`) so a half-written follow-up never has to live in the harness's own input.

*As built (2026-10-06; `vk-tui::desk`, `vk-tui::drafts`, `vk-tui::assist`, `vk-tui::gallery`):* all four are full pane-area views like the inbox (`Popup::Desk/Drafts/Assist/Gallery`), opened only by the user; none adds a default keybinding.

- **Session desk** (palette `desk`). Opens with the query line focused; `enter` runs `desk.search` with the chips (`R` repo — default the focused workspace's root, `h` harness any/claude/codex/pi, `D` date any/7d/30d; each change re-runs the search), `tab` switches to `desk.sessions`. Rows are `● live | ↻ resumable | · none` + `harness · repo · session · turn n · age · snippet`; the selected row shows its available actions labelled exactly (`[o] Focus live pane`, `[r] Resume native session`, `[c] Start new agent with context`). `enter` → `desk.open {session, turn}` (turn items rendered as role · kind + text; never focuses). `o` sends `desk.open focus=true` only for live rows. `r` shows the exact `resume.command` and a target: `[t]` a new tab in the session's workspace (default) or `[p]` the focused pane when it has no agent; `enter` → `desk.resume {mode: native, pane?}` (`session_live` / `pane_busy` explained). `c` loads `desk.context` into an editor; `ctrl+t` edits the turn selection (`3-5,7`, default last three) and reloads, `ctrl+a` toggles "also start <harness> in a free pane, without a prompt", `ctrl+x` saves via `desk.resume {mode: new_agent, start, turns}` and, when the package text was edited, `draft.update` with the edited text — it never sends. `F` forget asks first (`desk.forget {session}`). The coverage line comes from `desk.status` (sources, rows, opted-in roots per harness, exclusions, retention, pending bytes). Deviation: goto prefix `?` for conversations is not built.
- **Drafts** (palette `drafts`; agent peek `d` — the agent's workspace, sending to that agent by default; task details `D` — task-scoped drafts). Rows: selection mark, order, label (title or first line), `📎N`, last send state (`sending`, `✓ delivered`, `? unknown`, `✗ failed`) and a preview of the selected draft. Keys: `n` new, `e`/`enter` edit, `J/K` reorder (`draft.reorder`), `space` select, `m` combine (≥ 2, `draft.combine`), `x` delete (confirm), `s` send, `R` reconcile a `? unknown` send, `r` refresh, `tab` notes, `esc` back (to the peek/task details it came from). Editor: multi-line; `ctrl+x` save (`draft.create`, or `draft.update` with `expected_rev`; `draft_changed` is shown), `ctrl+f` attach a file (absolute path on the draft's machine; queued until save for a new draft), `ctrl+s` attach the newest screenshot (`screenshot.list {limit: 1}` → `{kind: screenshot, blob}`), `esc` discard (asks when edited). Deviation: `ctrl+x` saves because `ctrl+s` is the screenshot attachment; clipboard images are not attached yet.
  - **Send**: target picker over the workspace's live runs (default first), `draft.check` per target: `send path: prompt_input (<harness>) · steer: not supported`, or **Open pane to send** with the `unsafe` reason (enter then refuses locally), hidden attachments, `[x] include notes (i)`, and the exact text. `enter` dispatches `draft.send` through `App::mutate`: the call (with its idempotency key) is written to the per-client pending-operations file **before** dispatch and reconciled with `task.operation.get` after a link drop or restart (`:pending_operations` lists it as "Send draft"); unknown outcomes are never resubmitted. Replies: accepted → "Sending — delivered once the agent starts a turn with this text" (updated by `draft.delivered/delivery_unknown/send_failed` events); `send_unsafe` → "Not sent (zero bytes written; draft kept): <reason>" + `[o] Open pane to send` (focuses that pane — the only focus change); `reconcile_first` → `[R] reconcile first`; `delivery_unknown`/`already_delivered`/timeouts → `[!] send again anyway (the earlier message may have arrived)`, which sends with `retry_despite_unknown` and a fresh key. `R` → `draft.reconcile` (delivered / still uncertain with `may_retry`).
- **Notes** (palette `notes`, or `tab` in the drafts view): "Never sent unless you include it"; `e` edit, `ctrl+x` save with `expected_rev`; a conflict asks `[r] reload theirs (discard yours) · [o] overwrite with yours`; `notes.updated` reloads (or marks "changed elsewhere" while editing).
- **Peek**: the reply box (`r`) gains `ctrl+d` **Save as draft** (`draft.create` in the agent's workspace; back to the peek, nothing sent); `d` drafts, `p` screenshots (06 B8), `s` suggest title (14).
- Live updates: the TUI also subscribes to `draft.*`, `notes.updated` and `assistant.*` (07 §3 event push).
- **Assist actions** (14): Track form `ctrl+g` **Suggest task details** (the selected turn only), task details `S` **Summarize review**, agent peek `s` / palette `assist_pane_title` **Suggest title**, palette `assist_briefing` **Briefing**. Each runs `assistant.generate` and shows the exact preview (system and user text, model, adapter → endpoint host, execution machine, bytes, estimated tokens, max output, estimated max cost, redactions/omissions); nothing is sent until `y` (`assistant.confirm {request, preview_digest}`); `n`/`esc` cancels (`assistant.cancel`). The confirmed request is polled with `assistant.get` (about once a second) and refreshed on `assistant.request_*` events. Results are editable drafts only: suggested task details fill the Track form on `enter` (title, objective, constraints + criteria as optional criteria, stop point; marked "Suggested by the assistant", still unsaved — suggested checks are listed, not added); a review summary or briefing is editable text with `ctrl+d` save as draft (task / workspace scope); a suggested title is applied (`pane.rename`) only on `enter`. States: `disabled` → "Assistance is off — enable in config"; `consent_required`/`consent_invalidated`/`operation_not_granted`/`context_class_not_granted` → "Grant consent for this workspace?" with `[g]` calling `assistant.consent {workspace, operations: [op]}` (default classes) and regenerating; `not_configured` and older servers are explained. An `auto_send` request is shown as "Sent without confirmation".
- Tests (`vk-tui`, fake control-stream replies, never the host terminal or a model): `desk::tests`, `drafts::tests` (incl. pending-file persistence of draft-send keys and reconcile-on-reconnect), `assist::tests`, `gallery::tests`.

### 6.8 Recent targets, last workspace, hints and the terminal title (as built, research R5)

- **Per-client history** (`vk-tui::nav`): `<state>/<session>/nav-<client>.json` where `<client>` is `$VIBEKE_CLIENT_NAME` (sanitized) or `default` — so two laptops, or two named clients on one machine, keep separate histories. It holds the last 50 focused targets (machine label + pane/task id + workspace), the last 20 palette actions, and the current and previous workspace. Focus changes are observed once per frame.
- **`last_workspace`** (default `prefix+shift+l`; spec 08 had no binding and `prefix+l` is `focus_pane_right`) toggles to the previously focused workspace (across machines), landing on the pane this client last used there, else the workspace's first tab.
- **Hints** (`url_hints`, default `prefix+shift+u`; `prefix+u` stays `mark_unread`): labels (`a s d f j k l g h …`, two letters past 26) over every URL, `file:line[:col]` path, git SHA (7–40 lower-case hex with a digit and a letter), Vibeke handle (`w2:p1`, `b3`, `v4`, `#k12`, …) and ULID visible in the focused pane. A label **opens** URLs — loopback URLs as a browser pane next to the pane (that machine's `localhost`, window without graphics), other `http(s)` URLs with the local OS opener (`open`/`xdg-open`) — and **copies** everything else (OSC 52 / OS clipboard); `SHIFT+label` always copies. Any other key closes. `file:line` targets are not verified against the file system yet. *Plugin link handlers (M5 slice 3):* when the activated target (or the token under a Ctrl/Alt+click) matches a `[[link_handlers]]` pattern of a registered plugin (`plugin.link_handler.list`, refreshed with the hints and the palette), a chooser lists the matching handlers in registry/manifest order (untrusted ones disabled with the trust hint) above the default `Open`/`Copy`; `1..9`/enter pick, esc cancels. A handler runs with `plugin.link.open`, so its action gets `HERDR_PLUGIN_CLICKED_URL` and `HERDR_PLUGIN_LINK_HANDLER_ID`. `SHIFT+label` still copies without asking.
- **OSC 8 links** (as built, lane 1B, 03 §8): holding `ctrl` or `alt` over a pane link underlines every run of it (wrapped pieces included); `ctrl`/`alt`+click opens it — plugin link handlers first, then loopback URLs as a browser pane, other `http(s)` with the OS opener; `file:`, `mailto:` and other schemes are copied, never launched; targets with control characters or over 2 KiB are refused. Plain-text `http(s)` URLs are the fallback under `ctrl`/`alt`+click. On macOS Cmd+click belongs to the host terminal, which gets the links as OSC 8 when it supports them.
- **Terminal title sync**: `ui.title_sync = true` writes OSC 2 with `ui.title_format` (default `"{workspace} · {pane}"`; also `{tab}`, `{machine}`, `{session}`; the pane part is the agent's name/harness, else the pane title) whenever it changes; the host's own title is pushed (`CSI 22;2t`) before the first write and popped (`CSI 23;2t`) on exit. Control characters are stripped, 120 chars max. *Plugin window title (M5 slice 3):* a `client.window_title_changed` event (pushed; fetched with `compat.ui.state` after connecting) replaces the formatted title until `client.window_title.clear`; without `ui.title_sync` it is shown on the right of the tab bar instead. Control characters and bidi overrides are removed.

## 7. Notifications

### 7.1 Pipeline
Sources: agent state transitions (configurable which), interactions opened, bell, OSC 9/777/99 from panes, plugins, system (remote disconnected, update available).

```
notification.created → policy (rules, quiet hours, presence) → channels
   channels: toast (in-TUI)  |  host-terminal OSC (9 / 777 / 99)  |  native OS notifier  |  sound  |  bell
```

- **Presence**: a notification for a pane that is visible and focused in an attached client whose host terminal window is focused (focus events from the host, 03 §7.2) is suppressed except as a toast. `notifications.suppress_when_focused = true`.
- **Coalescing**: multiple notifications about one agent within `coalesce_ms` (3000) merge into one.
- **Native OS notifier**: macOS through a helper inside the signed bundle (`UNUserNotificationCenter`, so notifications carry the Vibeke identity and actions); Linux through `org.freedesktop.Notifications` over D-Bus; Windows toast in M6. Routing through the terminal loses click-to-focus. We support both and default to `native` when available, falling back to `osc`.
- **Click-to-focus**: native notifications carry `vibeke://focus?session=…&pane=…`. Clicking runs `vibeke focus <pane>`, which focuses the pane in the most recently active attached client and asks the OS to raise that host terminal window (macOS: activate the app by bundle id from `TERM_PROGRAM`/`__CFBundleIdentifier` captured at attach; Linux: `xdg-activation` token when available). Approval notifications on macOS also carry **Allow / Deny actions** that answer the interaction directly (only when `answer_channel = native` and the action's risk isn't `high`).
- **Inbound OSC from panes** (cmux-compatible): OSC 9 (iTerm2/ConEmu text), OSC 99 (kitty, incl. title/body/urgency), OSC 777 `notify;title;body` from any pane become `Notification{kind: osc}` attributed to the pane. Scripts and agents without an adapter can also run **`vibeke notify [--pane <id>|--current] [--urgency low|normal|high] <title> [body]`**, which works from any pane (and from a remote machine via the bridge).
- **Sound**: `notifications.sound = "default" | "none" | path`, configurable per kind.
- **Quiet hours**: `notifications.quiet_hours = "22:00-07:00"` (only `urgency = high` gets through).

*Implementation (M4, server side; `vk-server/src/notify.rs`):*
- **Pipeline.** Every `notification.created` passes through `notifications.on` (interaction → needs_approval/needs_answer, agent_state → done, bell, osc*), presence, quiet hours and `coalesce_ms` per pane, then the channels: `notifications.channels`, or derived from `channel` (`native` → toast + native, with OSC fallback when native is unavailable). The decision is recorded on the notification (`channels`, 07 §2.12).
- **Presence** is decided server-side: a TUI client with the pane focused and visible, whose `ViewHint.active` reports host focus.
- **Native notifiers.**
  - macOS: `terminal-notifier` when installed. Clicking runs `vibeke --session <s> focus <url>`.
  - macOS without it: `osascript display notification`. These notifications can't be clicked, because AppleScript notifications carry no action. **The signed helper bundle (`UNUserNotificationCenter`, Allow/Deny actions) is not built.**
  - Linux: `notify-send --action=default=Focus --wait` (freedesktop over D-Bus) in a background thread. Clicking runs `vibeke focus`.
  - `VIBEKE_NOTIFIER=none | auto | log:<file>` overrides the backend. Tests use `log:` or an injected notifier.
- **Native is not automatic yet.** It fires only once some client has reported `host` metadata (07 §2.1). Headless servers never pop OS notifications. **The TUI doesn't report it yet**, so until it does, native delivery needs `VIBEKE_NOTIFIER=auto`.
- **Click-to-focus.** `vibeke focus <pane | vibeke://focus?…>` (`client.focus`) switches to the URL's session and focuses the pane in the most recently active TUI client.
  - macOS raising: `osascript -e 'tell application id "<bundle>" to activate'`. The bundle id comes from the client's `host` metadata, else from the server's own `__CFBundleIdentifier`/`TERM_PROGRAM` (mapped for iTerm2, Terminal, Ghostty, WezTerm, kitty, VS Code, Warp, Alacritty…).
  - Linux raising (xdg-activation) is not implemented.
- **Sound and bell** are recorded as channels for clients; the server plays nothing.
- **As built (M4 TUI, `vk-tui::notifications`):**
  - `render.attach` carries `host: {bundle_id: $__CFBundleIdentifier, term_program: $TERM_PROGRAM}` — only to the server on this machine (a remote server can't raise this window and shouldn't pop OS notifications on its own host), so native delivery is automatic for local sessions.
  - The OSC 9 forward to the host terminal is skipped when `Notify.delivered` contains `native`, while the host window is focused, and for coalesced repeats.
  - Toasts coalesce per pane: another `Notify` for a pane whose toast is still up (or within `max(coalesce_ms, 1 s)`) updates that toast to `… (×n)` and extends it instead of stacking a new one.
  - The client still drops toasts for the focused pane while the host window is focused (`suppress_when_focused`); the server's presence filter only governs external channels.

### 7.2 Toasts
Stacked at the top-right of the pane area, max 3, auto-dismiss after 6 s (except interactions, which stay until answered or dismissed). `prefix+o` (`open_notification_target`) jumps to the newest toast's target.

## 8. Interaction cards (answering agents you're not looking at)

When an agent run has an open `Interaction` (02 §1.1) and **its pane is not focused**, Vibeke can show it as a **structured card**, independent of how the agent draws its own prompt. When you focus that pane, you see and answer the agent's own dialog instead (for held gate-mode hooks, focusing releases them so the native dialog appears — 04 §7.2).

`ui.interaction_overlay = "off" | "unfocused" | "always"` (default **`"unfocused"`**):
- `off`: no cards. The sidebar badge and notifications still show that an agent needs you; acting on them focuses the pane.
- `unfocused` (default): cards for unfocused agents only — inline in the sidebar row, in peek, in the inbox view, in notifications with actions, and as a popup when you explicitly invoke it (`prefix+a`, `a` in navigate mode/peek, clicking the badge or attention segment). Never for the focused pane; never popped up spontaneously.
- `always`: as `unfocused`, plus a card may be opened (on explicit invocation) for the focused pane's own interaction — for users who prefer one uniform card UI. Still never auto-opens over the focused pane.

```
┌ claude · samplehub/fix-login · needs approval ──────────────────── i42 ┐
│ Bash                                                    risk: medium   │
│   pnpm prisma migrate dev --name add_login_attempts                    │
│ cwd ~/.vibeke/worktrees/samplehub/fix-login                            │
│ reasons: writes database, network access                               │
│                                                                        │
│ [y] allow once   [s] allow for session   [r] add rule…   [n] deny      │
│ [e] deny with message…   [o] open pane   [esc] later                   │
└────────────────────────────────────────────────────────────────────────┘
```

- **Approval**: shows the tool, command/paths, a diff (scrollable, syntax-highlighted, `d` toggles full screen), risk and reasons. Keys: `y` allow once, `s` allow for session (only if the harness supports session scope natively), `r` creates a policy rule pre-filled from this interaction (02 §4) and lets you edit scope and regex before saving, `n` deny, `e` deny with a message (sent to the agent as the denial reason when the channel supports it).
- **Question** (AskUserQuestion, elicitation, pickers): each question as a list with `j/k` and `space` to toggle (multi-select) or `enter` (single). `tab` moves to the free-text field when `allow_free_text`. Submit `ctrl+enter`.
- **Plan review**: rendered markdown of `plan_md`, with `y` approve, `e` request changes with text, `n` reject.
- **Delivery**: the answer goes out through the interaction's `answer_channel`. `native` → adapter (hook response, extension, RPC). `keystrokes` → the adapter's verified key sequence (04: navigate, re-read the screen, confirm the cursor is on the target option, then press Enter; on mismatch, abort and show "couldn't deliver — answer in pane", jumping focus there). The card shows a delivery spinner, then ✓, or the failure with the reason (`interaction.answer_failed`).
- If the interaction is resolved elsewhere (in the agent TUI, another client or a rule), the card closes with a toast "answered in pane" or "answered by rule r3".
- Multiple open interactions: header `1/3`, `]`/`[` cycle. `A` in the card header opens the **batch view**. Fingerprints nominate candidates only: "allow all N" also requires equivalent effective policy scope, execution environment, operation and resource targets, known `risk ≤ medium`, native channels and live requests revalidated individually. Never batch questions, plan reviews or unknown/high-risk actions. Record and show each delivery independently, including partial failure. The task inbox uses this same rule (15 §8.3).

- *As built (Batch 2B, `vk-tui::batch`):* `A` on a card or the palette's `batch_approvals` opens a full pane-area view (`ui.interactions.batch = false` refuses it with a toast). Groups need ≥ 2 open interactions on unfocused agents that are approvals, answerable, `answer_channel = native`, risk low/medium, and equal on machine, harness, tool, the raw command (byte for byte: no whitespace normalization, since `echo a # rm x` and `echo a #\nrm x` differ only in whitespace but are different programs), sorted paths, execution environment (pane isolation level + network) and policy scope (workspace root) — the client-side form of 15 §8.3's rule; everything else is listed under "answer one by one" with the reason. A command containing `#`, a line break, `;`, `&&`, `||`, `|`, backticks or `$(` is never batched (batch 2 review; also `vk_review::attention::batchable`), and revalidation uses the same strict comparison (`batch_tests::commands_differing_by_comments_newlines_or_whitespace_never_share_a_batch`). Members start ticked (`space` unticks); `a`/`y` allow and `n` deny the ticked members of the selected group: each is revalidated against the current model right before sending (skipped with "no longer pending or changed" otherwise) and answered with its own `interaction.answer` and idempotency key. Each row then shows its own delivery state (`delivering…`, `✓ delivered`, `? unknown`, `✗ failed`, or the answer's error), so partial failure is visible; `o`/`enter` focuses a row's pane.
- *As built (`ui.interaction_overlay`, Batch 2B):* every explicit card opening (`prefix+a`, navigate `a`, peek `a`, the inbox) goes through one gate: `off` focuses the agent's pane instead (its own dialog answers) with a toast, `unfocused` refuses a card for the focused pane, `always` allows it.
- *As built (v1 TUI, `vk-tui::elevate`; 09 §3.2):* **elevation approval.** A program in a pane asking for elevated access (`vibeke auth elevate`, `auth.elevate`) never gets a prompt drawn over any pane. The TUI learns of requests from the pushed `auth.elevate_requested|granted|denied` events and from `auth.list` after every connect (requests made before the subscription; an older server answers `method_not_found` and shows nothing). While requests are open the tab bar's right cluster shows ` ⚿ w1:p3 (claude · api) asks for elevated access — prefix+shift+e ` (chrome only; the server's high-urgency notification toasts as usual). Nothing opens by itself: `elevation_requests` (default **`prefix+shift+e`**, or the palette) opens a review view that **replaces the pane area**, so the requesting pane's content is not on screen while the prompt is. It is titled `Vibeke · elevation request — drawn by Vibeke, not by any pane` and shows the pane (handle, agent, workspace, machine), the request id and age, the reason quoted as `Reason (written by the pane; unverified)` with control characters and bidi overrides removed, the expiry (`the request stays open until HH:MM (in 28m)` — the server forgets undecided requests after 30 minutes, and they drop here then too) and what approving grants (full API access for 10 minutes, from that pane only). `y` approves and `n` denies with `auth.elevate.decide {request, decision}` on the request's machine (one decision in flight per request); `j/k` select among several; `o` goes to the pane; `esc` closes without deciding. **Never auto-approved:** there is no setting that approves, keys in the first 600 ms after the view opens are ignored (they were typed for the pane), and `ctrl`/`alt`/`super` chords never decide. A TUI that is itself inside a Vibeke pane (`VIBEKE_PANE_TOKEN`/`VIBEKE_ELEVATED_TOKEN` in its environment, local machine) is not full scope: the view says so and shows the CLI command to run outside Vibeke instead of sending; a server refusal (`permission_denied`) is explained the same way, `not_found`/`conflict` (withdrawn, expired, decided elsewhere) drops the request.
- **Delivery states** (02/04): the card reflects `decision_recorded → delivering → delivered`, or `delivery_failed` / `delivery_unknown`. `delivery_unknown` (e.g. server crashed mid-delivery) never auto-retries; the card asks the user to check the pane, and the adapter reconciles with the harness before any resend.

**Focus acceptance (M1):** with any agent focused, an interaction arriving for that agent produces no Vibeke-drawn content over the pane and zero bytes written to its PTY; the agent's native dialog is what the user sees (gate-mode hooks released within 200 ms of focus). Cards for unfocused agents answer natively without changing focus.

**Acceptance is by tested capability, not by harness name.** Each (harness version × launch mode × interaction kind) has a capability record in the harness capability matrix (04): `observe`, `answer_native`, `answer_keystrokes`, `reconcile`.
- **M1:** for every matrix cell marked `answer_native` (expected: Claude Code `PermissionRequest`; Codex hooks `PermissionRequest`; pi extension dialogs via the uiContext wrapper where golden-tested — pi has no Vibeke gate, 04 §6.3), answering from a card sends zero keystrokes to the agent TUI, the agent proceeds within 300 ms p95, and a server kill between decision and delivery ends in `delivered` or `delivery_unknown`, never a duplicate decision.
- For cells marked only `answer_keystrokes` (e.g. questions where native answering is unverified), verified keystroke delivery succeeds on the golden corpus and fails safe (no Enter) on every mismatch fixture; the UI labels the delivery as best-effort.
- Cells marked `observe` only: the card shows the interaction and a "jump to pane" button; no answer is attempted.

## 9. Onboarding

First run (`onboarding = true` or no config):
1. **Detect** host terminal capabilities (03 §6.1) and print a short pass/warn table (keyboard protocol, graphics, clipboard, notifications).
2. **Import from Herdr?** If `~/.config/herdr/config.toml` exists: "Import keybindings, theme, sidebar rules and worktree dir? [Y/n]". A running Herdr session can also be recreated (layout plus cwd plus agent resume) via `vibeke import herdr --session` (§12).
3. **Agent integrations**: detect installed harnesses (claude, codex, pi, omp, opencode, gemini, …) with versions, and offer `vibeke integration install <name>` for each, showing exactly which files will be modified (diff preview) (04).
4. **Notifications**: choose native / terminal / none, then send a test notification and confirm the click focuses Vibeke.
5. **Theme**: pick from the built-ins, with auto light/dark.
6. Write `~/.config/vibeke/config.toml` with only the non-default choices, and set `onboarding = false`.

Everything onboarding does is also available later from `vibeke setup` and the palette.

*As built (Batch 2B, `vk-tui::onboarding`, `vibeke/src/setup.rs`):* the TUI opens a full pane-area view by itself when `onboarding = true` or the config file doesn't exist, on a client whose first machine is local; `esc` closes it for this run (it returns next time until a config is written), palette `setup` (also `settings`, `prefix+s`) reopens it. Steps: terminal pass/warn table from the startup probe (keyboard, graphics, clipboard, notifications incl. the native notifier found, colour, sync updates); Herdr import (only when `~/.config/herdr/config.toml` exists; its non-default keys become the base of the written file); integrations — every harness with its binary on `PATH`, install state and the planned diff (`d`), selected with `space`, installed only on `i` then `y` after the exact file list is shown (re-planned at install time; `CLAUDE_CONFIG_DIR`/`CODEX_HOME`/`VIBEKE_*_HOME` redirect it); notifications (native/terminal/none, `t` sends `notification.send` as a test); theme (built-ins `catppuccin`, `catppuccin-latte`, `terminal`, previewed live; `a` toggles `theme.mode = "auto"`); write — a preview of the file, then an atomic 0600 write that keeps existing keys and comments (`toml_edit`), sets `onboarding = false` and only the non-default choices, and is refused if the loader would reject it. `tab`/`shift+tab` move between steps. `vibeke setup` runs the same steps on stdin (`[y/N]` prompts; no install without a yes; consent is never assumed: end of input or a read error at any question is "no" and stops setup without writing anything further, never a prompt's default, and without a terminal on stdin nothing is written unless `--yes`; batch 2 review, `setup::tests::eof_and_closed_stdin_never_write_the_config`, `onboarding::setup_without_a_terminal_or_yes_writes_nothing`), or non-interactively with `--yes --install claude,codex|all --notifications … --theme … --import-herdr`; `--dry-run` prints the diffs and the config and writes nothing. Not built: confirming that a clicked test notification focused Vibeke (the user is only asked).

## 10. Keybindings

### 10.1 Syntax (tmux-style)
```
binding   := [ "prefix+" ] chord ( " " chord )*        # space-separated sequence after prefix, e.g. "prefix+g w"
chord     := ( modifier "+" )* key
modifier  := ctrl | shift | alt | super | cmd | hyper | meta | altgr
key       := a-z | 0-9 | f1..f24 | enter | tab | esc | backspace | space | up | down | left | right | home | end
           | pageup | pagedown | insert | delete | named punctuation (minus, comma, period, slash, backslash,
             semicolon, quote, backtick, lbracket, rbracket, equal, plus, ampersand, colon, question, …)
           | literal single printable char (e.g. "[", "?")
range     := "1..9"   (indexed bindings: switch_tab = "prefix+1..9")
```
- `"prefix+n"` requires the prefix. A chord without `prefix+` is a **direct** binding active in terminal mode and is consumed before the pane sees it. Direct bindings warn at config load if they shadow common app keys (`ctrl+c`, `ctrl+d`, `ctrl+r`, `esc`).
- Matching uses the base-layout key (03 §7.1), so bindings are layout-independent. `cmd`/`super` bindings need a host that reports them (kitty keyboard); config load warns otherwise.
- Prefix behaviour: `keys.prefix_timeout_ms = 1500`; pressing the prefix twice sends the prefix key to the pane (tmux-like, `keys.prefix_passthrough = true`).
- Empty string `""` unbinds. `vibeke keys list` prints the effective keymap; `vibeke keys check` reports conflicts.

### 10.2 Default keymap
Vibeke additions beyond the base set are marked ✚.

| Action | Default | | Action | Default |
|---|---|---|---|---|
| help | `prefix+?` | | split_vertical | `prefix+v` |
| settings | `prefix+s` | | split_horizontal | `prefix+minus` |
| detach | `prefix+q` | | close_pane | `prefix+x` |
| reload_config | `prefix+shift+r` | | zoom | `prefix+z` |
| open_notification_target | `prefix+o` | | resize_mode | `prefix+r` |
| workspace_picker / navigate | `prefix+w` | | toggle_sidebar | `prefix+b` |
| goto | `prefix+g` | | focus_pane_left/down/up/right | `prefix+h/j/k/l` |
| new_workspace | `prefix+shift+n` | | cycle_pane_next / previous | `prefix+tab` / `prefix+shift+tab` |
| new_worktree | `prefix+shift+g` | | edit_scrollback | `prefix+e` |
| rename_workspace | `prefix+shift+w` | | ✚ enter_copy_mode | `prefix+[` |
| close_workspace | `prefix+shift+d` | | ✚ paste_buffer | `prefix+]` |
| new_tab | `prefix+c` | | ✚ command_palette | `prefix+:` |
| | | | ✚ inbox | `prefix+i` |
| | | | ✚ next_attention_focus | `prefix+shift+a` |
| rename_tab | `prefix+shift+t` | | ✚ search_scrollback | `prefix+/` |
| previous_tab / next_tab | `prefix+p` / `prefix+n` | | ✚ next_attention | `prefix+a` |
| switch_tab | `prefix+1..9` | | ✚ mark_unread | `prefix+u` |
| close_tab | `prefix+shift+x` | | ✚ pin_pane | `prefix+alt+p`* |
| rename_pane | `prefix+shift+p`* | | ✚ float_new / toggle_floats | `prefix+f` / `prefix+shift+f` |
| remote_image_paste | `ctrl+v` (remote only) | | ✚ sync_input | `prefix+shift+s` |
| | | | ✚ new_task | `prefix+shift+k` |
| | | | ✚ preview_list / open | `prefix+shift+o` |
| | | | ✚ last_workspace (§6.7) | `prefix+shift+l` |
| | | | ✚ url_hints (§6.7) | `prefix+shift+u` |
| | | | ✚ sync_input_pane (§5) | `prefix+alt+s` |
| | | | ✚ fleet (§6.6) | `prefix+shift+m` |
| | | | ✚ agent_list (§6.5) | `prefix+alt+a` |
| | | | ✚ elevation_requests (§8) | `prefix+shift+e` |
| | | | ✚ batch_approvals / setup / trust_repo | palette (unbound) |
| | | | ✚ tab_renumber / task_recreate / task_forget | palette (unbound) |

✚ `search_global` (search all panes, popup) defaults to `prefix+alt+/` (`prefix+/` is `search_scrollback`); M4 palette-only actions: `float_pane`, `embed_pane`, `group_new`, `group_move`, `group_rename`, `group_collapse`, `layout_save`, `layout_apply`, `status_bar_toggle`, `theme_detect`.

Action names: the copy-mode binding is `enter_copy_mode` and the picker is `workspace_picker`, because `[keys.copy_mode]`/`[keys.navigate]` are tables (Goal 01 deviation).

**Previews and browser panes (06 B2/B3.2; Goal 03 Stage 2).** A preview whose page threw an uncaught exception or called `console.error` (in an agent's headless session or in a browser pane on it) shows a red ` !N` after its label on the tab-bar chip and the sidebar row until it is opened (`preview.console_error`, 06 B5). With `ui.sidebar.preview_thumbnails = true` and kitty graphics, local previews' rows also show a tiny 4x1-cell thumbnail of their newest screenshot (06 B8). `prefix+o` (`open_notification_target`) keeps its meaning while a toast with a target is showing; otherwise it opens the focused pane's preview as a browser pane next to it (a window when the host has no graphics); `open_preview` is the same action without the toast rule. **Preview chips and sidebar rows (as built):** left click opens the preview as a pane; right click targets it for the palette (filter `preview_`): `preview_window`, `preview_proxy` (opens the one-time proxy URL in your normal browser, only on that action), `preview_mirror` / `preview_unmirror` for remote previews. A mirrored preview shows the warning badge `⇄ :5173 mirrored` (bold yellow, replacing the port/label) in its row and chip until unmirrored (06 B4). While a **browser pane is focused**, a browser table is consulted before the global one; it overrides keys that mean nothing in a browser pane (copy mode, scrollback editing, sync input). Rebind with `[keys] browser_back = "…"` etc.

| Action (browser pane focused) | Default | Overrides |
|---|---|---|
| ✚ browser_address (edit URL) | `prefix+e` | edit_scrollback |
| ✚ browser_back / browser_forward | `prefix+[` / `prefix+]` | enter_copy_mode / paste_buffer |
| ✚ browser_reload / browser_hard_reload | `prefix+.` / `prefix+,` | — |
| ✚ browser_screenshot | `prefix+shift+s` | sync_input |
| ✚ browser_window (open in window ⇄ back to pane, 06 B3.3) | `prefix+o` | open_notification_target |
| ✚ browser_console (console/network split under the pane; again closes it) | `prefix+alt+c` | — |
| ✚ browser_paste_image (clipboard image into the page, 06 B3.2) | `prefix+shift+v` | — |
| ✚ browser_take_over (watch pane: take over ⇄ release the agent session, 06 B7) | `prefix+t` | — (unbound globally) |

All other keys go to the page except the prefix; direct (non-prefix) bindings such as `ctrl+v` don't apply in a browser pane. Mouse: click the chrome's ←/→/⟳ or its URL; Ctrl/Alt+click a `http://localhost:<port>` URL printed in any pane opens it in a browser pane next to that pane.

\* `rename_pane` is bound to `prefix+shift+p`, so we default `pin_pane` to `prefix+alt+p` (§2.3 references to "pin" use this binding). `vibeke keys check` must report no conflicts on the shipped defaults (CI test).

Mode-local keymaps: `[keys.navigate]`, `[keys.copy_mode]`, `[keys.resize]`, `[keys.card]`. All are rebindable.

### 10.3 Custom commands
`[[keys.command]]` (`type = "shell" | "pane" | "popup" | "plugin_action"`, `width`/`height`, `description`) plus ✚ `type = "float"` (persistent floating pane), ✚ `cwd = "pane" | "workspace" | path`, ✚ `env`, ✚ `title`, and ✚ `when = "agent:claude"` (only active when the focused pane runs that harness). *As built (M5 slice 3):* `type = "plugin_action"` with `command = "<plugin>.<action>"` runs the plugin action in the focused context (`plugin.action.run {action, pane, source: keybinding}`); its `description` (else `title`) labels the toast and the palette entry, which shows the binding. Plugin manifests' own `[[keys.command]]` defaults are installed while the plugin is trusted and enabled (the server lists them with conflicts already resolved: a key that duplicates or shadows any user, default or earlier-plugin binding is skipped and reported in `plugin.list`; user keys win), and removed when it is disabled or unlinked; the action-context rule also applies to key bindings (a binding fired where the action does not apply toasts `not available here`). A plugin's `agent.view.set` line shows after the run's state in the sidebar row and in the peek.

## 11. `config.toml` schema

**This section is the single canonical configuration reference.** Other sections describe behaviour and may show excerpts; key names here win. CI generates this block from the Rust config types and fails if any section's excerpt uses a key not in the schema.

Location: `~/.config/vibeke/config.toml` (override with `VIBEKE_CONFIG`). Unknown keys produce warnings, never errors. The JSON Schema is generated from Rust types (`vibeke config schema`) for editor completion. `vibeke --default-config` prints a fully commented default file.

```toml
onboarding = false

[theme]
mode        = "auto"                  # auto (follow the host terminal's reported light/dark) | light | dark
name        = "catppuccin"            # built-ins: catppuccin(-latte), terminal, tokyo-night, dracula, nord, gruvbox,
                                      # one-dark, solarized, kanagawa, rose-pine, vesper + any themes/*.toml
auto_switch = true
dark_name   = "catppuccin"
light_name  = "catppuccin-latte"
[theme.custom]                        # token overrides: panel_bg, fg, muted, accent, red, yellow, green, blue, border, selection…
accent = "#f5c2e7"
[theme.pane]                          # default pane palette overrides (ansi0..15, fg, bg, cursor) — propagated per 03 §10.4
bg = "reset"

[terminal]
default_shell        = ""             # "" → $SHELL → /bin/sh
shell_mode           = "auto"         # auto | login | non_login
new_cwd              = "follow"       # follow | home | current | <path>
term                 = "xterm-256color"
scrollback_lines     = 10000
archive_scrollback   = true
archive_styles       = false
archive_max_per_pane = "200MiB"
archive_days         = 30
grapheme_width       = "auto"         # auto | unicode | legacy
allow_passthrough    = false
host_overrides       = {}             # e.g. { kitty_graphics = false } for misreporting hosts
                                      # keys: kitty_keyboard sync_update truecolor undercurl osc52 osc8 focus_events
                                      # sgr_mouse bracketed_paste sixel kitty_graphics kitty_shm iterm2_images sgr_pixels
[terminal.env]                        # extra env injected in every pane
EDITOR = "nvim"

[clipboard]
osc52_write       = "allow"           # allow | deny
osc52_read        = "deny"            # deny | ask | allow — OSC 52 reads (03 §8); never answered without a toast
copy_on_select    = true              # a finished mouse selection is copied (false: stays in copy mode for y)
primary_selection = false
mouse_select_in_apps = "modifier"     # modifier (shift/alt+drag over mouse-reporting apps) | always (every drag selects; apps get clicks and wheel)
remote_write      = "ask_once"        # ask_once (per machine) | allow | deny — OSC 52 writes from remote panes (06 A9, 09 §7)

[paste]                               # 06 A11 — translate dropped/pasted local paths for panes that can't see them
translate        = "paths_only"      # paths_only | embedded | ask | off
max_auto_bytes   = "50MiB"           # larger drops and any directory ask first
inbox_retention  = "14d"

[keys]
prefix             = "ctrl+b"
prefix_timeout_ms  = 1500
prefix_passthrough = true
altgr_mode         = "auto"           # auto | text | chord
shift_enter_legacy = "cr"             # cr | lf
# … action = "binding" entries as in §10.2
[keys.copy_mode]
mode = "vi"                           # vi | emacs
editor_include_ansi = false           # edit_scrollback's editor file keeps colours (in-memory rows), 03 §11.3
# per-key overrides…
[[keys.command]]
key = "prefix+alt+g"
type = "popup"
command = "lazygit"
width = "80%"
height = "80%"

[ui]
interaction_overlay        = "unfocused"   # off | unfocused | always — cards only for agents you're not looking at (§8)
max_fps                    = 120
background_animation_fps   = 4
animate                    = true
focus_follows_mouse        = false
focus_follows_mouse_delay_ms = 120
confirm_close              = "running"
marked_unread_clears_on_focus = false
title_sync                 = true          # outer terminal title (OSC 2), §6.7
title_format               = "{workspace} · {pane}"   # also {tab}, {machine}, {session}
[ui.sidebar]
attention_section = true              # "needs you" rows on top (§2.1)
position          = "left"            # left | right
width             = 28
min_width         = 18
max_width         = 48
auto_width        = true
collapsed         = false
show_shell_panes  = false
nest_tasks        = true
preview_thumbnails = false             # tiny latest-screenshot thumbnail per preview row (kitty graphics; 06 B8)
show_state_source = "inferred-only"   # inferred-only | always | never
[ui.sidebar.isolation_glyphs]
host = ""
sandbox = "sb"
container = "ct"
vm = "vm"
[[ui.sidebar.token]]                  # token rules
match = { harness = "codex" }
label = "cx"
[ui.tabs]
position     = "top"                  # top | bottom | hidden
show_numbers = true
[ui.status_bar]
enabled  = false
position = "bottom"
left     = ["machine", "task", "branch"]
center   = ["attention"]
right    = ["agents_summary", "clock"]
[ui.sync_input]
include_agents = false
[ui.interactions]
batch     = true                      # equivalent native approvals; fingerprint alone is insufficient (§8)
[ui.fleet]
tile_view = "terminal"                # terminal (live miniature of the agent's own UI) | timeline

[notifications]
channel               = "native"      # native | osc | both | none
sound                 = "default"
suppress_when_focused = true
coalesce_ms           = 3000
quiet_hours           = ""            # "22:00-07:00"
channels              = []            # toast | native | osc | sound | bell; [] = derived from `channel` (§7.1)
[notifications.on]                    # which events notify
needs_approval = true
needs_answer   = true
done           = true
error          = true
bell           = false
osc            = true
remote_disconnected = true

[layouts.dev]                         # named layouts (07 §2.14 LayoutSpec): `vibeke layout apply dev`,
cwd = "~/code/app"                    # `vibeke workspace create --layout dev`
[[layouts.dev.tab]]
title = "edit"
pane = { split = "right", children = [{ run = "nvim ." }, { run = "npm run dev" }] }

[agents]                              # see 04 for harness manifests and adapter options
auto_detect       = true
shims             = true              # prepend vibeke shim dir to PATH in panes (e.g. codex → per-pane app-server); per-harness opt-out below
resume_on_restart = "ask"             # ask | always | never
name_from_task    = true
[agents.harness.claude]
enabled = true
integration = "hooks"                 # hooks | stream-json | screen
extra_args = []
[agents.harness.pi]
integration = "extension"             # extension | rpc | screen
[agents.harness.codex]
shim = true                           # adds --disable daemon_auto_start; user args untouched
headless_shared = false               # one app-server per Vibeke session multiplexing threads (04 §6.2)

[policy]                              # rules: 02 §4
[[policy.rule]]
match  = { tool = "Bash", command_regex = '^(pnpm|npm) (test|run lint)( |$)' }
effect = "allow"

[tasks]                               # see 05
root              = "~/.vibeke/worktrees"   # or "sibling" → ../<repo>-<slug>
vcs               = "auto"            # auto | git — VCS detection (jj support was removed for v1 (2026-10-06); git worktrees only)
checkout          = "auto"            # auto (worktree) | worktree | clone | none (05 §4; clone is the default for container/vm, 13 §6)
branch_template   = "{user}/{slug}"
fetch_before_create = true
default_agent     = "claude"
port_pool         = "20000-29999"     # machine-wide pool shared by all sessions (05 §6)
port_block        = 10                # ports per task lease
setup_script      = ".vibeke/setup.sh"
copy_files        = [".env", ".env.local"]
[tasks.cleanup]
on_finish     = "keep"                # keep | archive | remove
stale_after   = "14d"
auto_gc       = false
protect_dirty = true                  # never remove a checkout with uncommitted changes without --force
[tasks.best_of_n]
suffix = ""                           # optional per-run prompt suffix for best-of-N runs (05 §12, post-1.0)

[collision]                           # advisory only (05 §10)
enabled        = true
window         = "30m"
fs_attribution = "auto"               # auto | off | aggressive (fanotify)
enforce_claims = false                # courtesy guardrail for cooperating adapters only

[isolation]                           # see 13
default             = "host"          # host | sandbox | container | vm — for tasks without --isolate
yolo_default        = "auto"          # auto (vm → container → sandbox, first available) | sandbox | container | vm | host
confirm_host_yolo   = true            # confirm when *Vibeke* launches a yolo run on the host; user-typed yolo is never blocked
suggest_sandbox_for_yolo = false      # sidebar hint "relaunch in a sandbox?" for user-typed host yolo
network             = "dev"           # offline | dev | open
idle_suspend        = "30m"
container_provider  = "auto"          # auto | apple-container | orbstack | docker | podman | docker-sandbox
vm_provider         = "auto"          # auto | lima | tart | firecracker | cloud-hypervisor
[isolation.limits]
cpus = 4
memory = "8G"
disk = "30G"

[preview]                             # see 06
auto_discover     = "suggest"         # suggest | promote | off — discovered ports are suggestions; declared previews are authoritative
mode              = "pane"            # pane (live browser pane in the layout via kitty graphics, 06 B3.2) | window (Vibeke browser profile window, 06 B3.3); both route via SOCKS through the bridge so localhost works as-is | proxy (authenticated *.vibeke.localhost origins, 06 B4)
profile_browser   = "auto"            # auto | chrome | chromium | edge | brave | firefox
profile_scope     = "machine"         # machine | task — one profile per machine, or per task
profile_route     = "loopback"        # loopback | remote — where non-preview traffic of the profile exits
local_browser     = "profile"         # profile | default — what `vibeke preview open` launches
proxy_port        = 47800             # proxy mode listener (loopback only)
tls_origin        = false             # proxy mode: serve https://*.vibeke.localhost with a local CA (never auto-trusted: `vibeke preview trust-ca`)
browser_path      = ""                # remote headless browser binary ("" = auto-detect Chromium)
browser_idle      = "10m"             # stop the headless browser after this idle time
browser_external  = "subresources"    # deny | subresources | allow — non-preview destinations for the headless browser
browser_allow_private = []            # extra CIDRs/hosts the headless browser may reach (default: declared previews only)
default_viewport  = "1440x900"
screenshot_format = "png"
inline_thumbnails = true              # kitty-graphics thumbnails in the TUI where supported

[remote]                              # see 06
[[remote.machine]]
label   = "devbox"
address = "demo@devbox.tailnet"
transport = "ssh"                     # ssh (quic: post-1.0)
keybindings = "local"                 # local | server
auto_connect = true
auto_upgrade = false                  # upgrade the remote vibeke on connect without asking
bootstrap   = "push"                  # push (verified artifact from this machine) | remote-download (remote fetches, verified by signature) — 06 A3, 09 §7
[remote]
input_when_offline = "drop"           # drop | ask — keystrokes to a pane whose machine is offline
predictive_echo    = "auto"           # auto | always | never (QUIC only, post-1.0)

[pane]
size_policy = "latest"                # latest | smallest | pinned — which client holds a pane's geometry lease (03, 07 §3)

[render]
max_unacked = 2                       # in-flight diffs per pane before the server falls back to a full frame (07 §3)

[security]                            # see 09
encrypt_state = false                 # encrypt blobs + scrollback segments at rest with a key in the OS keychain (protects backups, not same-UID processes)

[desk]                                # session desk conversation index (07 §2.14a)
index          = true                 # index transcripts of runs Vibeke has seen
roots          = {}                   # opt-in extra transcript dirs: { claude = ["~/.claude/projects"], codex = ["~/.codex/sessions"] }
exclude        = []                   # paths / cwds / repos never indexed ("/dir" = dir and below, "prefix*")
retention_days = 90
interval_s     = 15
pass_bytes     = 8388608              # bytes read per indexing pass

[plugins]                             # see 07
enabled = ["acme.example"]

[compat]
herdr_env    = true                   # export HERDR_* aliases in panes
herdr_socket = false                  # expose the full public Herdr API for the tested baseline (M5)

[update]
channel        = "stable"             # stable | preview
version_check  = true
manifest_check = true                 # signed harness-manifest channel (04 §13)
```

### 11.1 Repo-local config
`.vibeke/config.toml` in a repo root can set `tasks.*`, `preview.*`, `policy.rule` (scoped to the repo) and `[[keys.command]]`. It is ignored until trusted (`vibeke trust` or the onboarding prompt shown when you first open the workspace). The trust record is a hash of the file, so any change requires re-trust.

*As built (Batch 2B; `vk-config::repo`, server `repo_config.rs`, `vk-tui::trust`, `vibeke trust`):* trust is the existing `policy.trust` record (blake3 of the whole `.vibeke/` tree, 09 §4), so an edit anywhere in `.vibeke/` needs a new review. `policy.trust {path, check: true}` reports the repo, the file, its text, the digest, `trusted`, the parsed commands and the warnings without recording anything. Parsing keeps `[tasks]`, `[preview]`, `[[policy.rule]]` (only `deny`/`ask`; `allow` rules are dropped with a warning — repo policy can only tighten) and `[[keys.command]]`; any other section is ignored with a warning. `vibeke trust [path] [--check] [--yes]` prints all of that and, after a `y` (or `--yes`), records trust with the reviewed digest (a file changed in between is refused). The TUI checks each workspace's root once when it is first focused; an untrusted file gives a one-time toast pointing at `:trust_repo`, which shows the same review and trusts on `y`. Once trusted, the repo's commands appear in the palette as `Repo command: <title>` and their keys are bound (user and default bindings win) and work while that repo's workspace is focused; `[tasks]` keys are layered over the user's `[tasks]` for `task.create` in that repo (e.g. `copy_files`). Running a repo command re-checks trust with the server first (`policy.trust {check: true}`, batch 2 review): only if the tree is still trusted at the digest the user reviewed and still defines that exact command does it spawn; otherwise (a checkout rewrote the script it runs, an agent edited `.vibeke/`) nothing runs, a toast says it changed and the review opens for a fresh `y` (`trust_tests::a_changed_trusted_repo_needs_a_fresh_review_before_its_command_runs`). Accepted repo `[[policy.rule]]` entries are evaluated by the policy engine while the repo is trusted (tighten only, 09 §4). Not wired yet: repo `[preview]` keys.
- *As built (v1 TUI, `vk-tui::repo_preview`):* a trusted repo's **`[preview]`** is layered over this client's `[preview]` (repo keys win key by key; an invalid repo value keeps the user's for that key) when the trust check comes back trusted, and applies to that workspace's previews (the preview's pane's workspace, else its task's, else the focused one): `mode` decides what opening a preview does (`pane` browser pane, `window` profile window, `proxy` the proxy URL in the normal browser — chips, sidebar rows, goto entries and `open_preview`), `pane_split` where the browser pane goes (`right`, `down`, `tab`, `float`), and `inline_thumbnails = false` turns that repo's sidebar thumbnails off. Untrusted or changed files never apply; the user's own `mode`/`pane_split` now apply the same way (they were ignored before). Server-side preview keys (discovery, browser binaries, egress, proxy port) are read by the server from its own config, not from the repo file.

### 11.2 Hot reload
- `prefix+shift+r`, `vibeke config reload`, or file-watch (`config.watch = true`, default) → parse → validate → diff → apply.
- Applies live: theme, keys, ui.*, notifications, policy, preview, plugins enable/disable, sidebar tokens.
- Applies to new panes only: terminal.default_shell, shell_mode, term, env. A toast says so.
- On parse or validation error: keep the old config, show a toast with `file:line: message`, and `vibeke config check` prints the details. Never apply partially.
- Emits `session.config_reloaded { changed_keys }`.

## 12. Herdr importer

`vibeke import herdr [--config] [--session] [--dry-run]` (also offered in onboarding).

| Herdr | Vibeke | Notes |
|---|---|---|
| `onboarding` | `onboarding` | |
| `[theme] name/auto_switch/dark_name/light_name/[theme.custom]` | same keys | Built-in theme names are identical. |
| `[terminal] default_shell/shell_mode/new_cwd` | same | |
| `[update] channel/version_check` | same | `manifest_check` → `update.manifest_check` |
| `[keys] prefix` + every action key | same action names | Herdr's legacy `[keys.indexed]` → `switch_tab`/`switch_workspace`/`focus_agent` ranges. Bindings using `cmd`/`super` warn if the host lacks kitty keyboard. |
| `[[keys.command]]` | same | Windows `cmd.exe` semantics preserved on Windows hosts only. |
| `[worktrees] directory` | `tasks.root` | |
| `[ui] sidebar_width/min/max/collapsed` | `ui.sidebar.*` | |
| sidebar token rules (`hide = true` etc.) | `[[ui.sidebar.token]]` | 1:1 schema. |
| `remote_image_paste` | same | |
| `session.json` workspaces/tabs/panes/layout/cwd | recreated layout | `--session` only. |
| `agent_session {agent, value}` per pane | `resume_on_restart` candidates | Offered for resume via harness resume argv (04). |
| Herdr hook integrations (`~/.claude/hooks/herdr-agent-state.sh`, `~/.codex/hooks.json`) | left untouched | Vibeke's integrations install alongside. `HERDR_*` env aliases keep Herdr's scripts harmless (they report to the compat socket if enabled, else exit 0). |
| Plugins (Herdr registry, manifests, config/state) | `vibeke import herdr` inventories; `vibeke plugin install` / `link` activates in M5 | Full unchanged-plugin contract (07 §7.7): legacy trust consent, copy-based state migration, exact hooks/panes/context/API behavior. Missing baseline support blocks compatibility certification. |

The importer prints a report of mapped, defaulted and unsupported keys, and never overwrites an existing Vibeke config without `--force` (it writes `config.imported.toml` instead).

**Acceptance (M1 config + session):** importing a real `~/.config/herdr` (config + session.json with several workspaces) recreates every workspace and tab layout with correct cwds, and offers to resume all 7 Claude/Codex agents by their stored session ids.

## 13. Capability checklist

| Capability | Vibeke | Milestone |
|---|---|---|
| Workspaces / tabs / panes, splits, zoom, resize | §5 | M1 |
| Sidebar with agent states | §2, separate execution vs attention state (02) | M1 |
| Persistent server, detach/attach, named sessions | 01 §1, holders | M1 |
| Multiple clients on one session | render stream per client, geometry controller lease (03) | M1 |
| Notifications (terminal-routed) | §7 OSC out + OSC 9/99/777 in + `vibeke notify` | M1 |
| Native notifications, click-to-focus | §7 | M4 — server pipeline, notifiers and `vibeke focus` built; TUI sends `host` in `render.attach`, skips its OSC forward for `delivered: [native]`, coalesces toasts (§7.1); signed helper bundle open |
| Keybindings with prefix, custom commands (shell/pane/popup) | §10 | M1 |
| Themes | §11 | M1 (fixed), M4 (auto light/dark + propagation) — built. Server: `theme.mode`, `client.appearance`, `theme.changed`, `SessionModel.appearance`, `COLORFGBG`/`VIBEKE_THEME` in new panes, and panes' OSC 10/11/12/4 colour queries answered by their VT engine from the appearance's palette (mocha/latte), live panes included — the TUI never answers them, so there is one reply. TUI (`vk-tui::appearance`): the startup probe asks OSC 11 + `CSI ? 996 n` (the 997 report wins), reports `client.appearance {dark, source}` to every connected machine (again after a reconnect), and themes the chrome with `dark_name`/`light_name` (`theme.mode` forced values win; else this host's detection; else the server's `appearance`). Built-in chrome themes: `catppuccin`, `catppuccin-latte`, `terminal`. Mode 2031 is **not** enabled: crossterm can't parse unsolicited `CSI ? 997 ; n n` reports and would swallow keystrokes after one; instead the host is re-queried (event reader paused, replies read raw, ≤ 150 ms) when its window regains focus (at most every 3 s, `mode = auto` only) and from the palette (`theme_detect`) |
| Copy mode | 03 §11 | M1 (vi keys, `/` in buffer), M4 (archive search, edit scrollback) — server side built (`search.query`, `pane.read {source: archive}` paging by absolute line, `mem_first` in its reply). TUI built (`vk-tui::search`): entering copy mode asks `pane.read {lines: 0}` for the archive's `first` line and `mem_first`; `FetchHistory` pages only in-memory rows and older rows come from `pane.read {to, lines: 2000}`, stopping at the archive's first row (servers without `mem_first` keep the old all-`FetchHistory` paging). `/`, `?` or `n` with no match in the loaded rows sends `search.query {q, pane}`, picks the nearest hit older than the loaded rows, loads up to it with `pane.read` (5000-row pages, at most 200 000 rows, else a "too far" message) and puts the cursor on the match; no hit → `not found: q (whole history)`. Global search popup (`search_global`, `prefix+alt+/`, palette): `enter` runs `search.query` on every connected machine and lists hits (`[machine] handle title ▤/↑/▣ text`, closed panes marked); `enter` on a hit focuses the pane and opens copy mode on the hit's absolute line. Edit scrollback built (M4 TUI, `vk-tui::scrollback`): `edit_scrollback` (`prefix+e`, palette; from copy mode via a `[keys.copy_mode]` key bound to `edit_scrollback`) loads the focused pane's whole history with `pane.read {source: archive}` (newest page first, 5000-row pages, at most 200 000 rows, "older lines not loaded" beyond), joins soft wraps and opens a **read-only in-TUI viewer** at the live screen (or copy mode's view): `j k`, `space`/`b`, `ctrl+d/u`, `g G`, `/` with `n`/`N` (smart case, matching lines highlighted), `esc`/`q`. `e` opens the same text in `$VISUAL`/`$EDITOR` (split on whitespace; `+N` for vi/vim/nvim/nano/emacs/kak/micro…, `file:N` for helix): a new file in a private per-user temp dir (0700, ownership and mode checked) made read-only (0400); the TUI stops reading input, restores the host terminal, runs the editor in the foreground, takes the terminal back, repaints and deletes the file. Remote panes work the same (the text comes to this client). *v1:* `[keys.copy_mode] editor_include_ansi = true` keeps colours in the editor copy: before the editor opens, the in-memory rows are fetched styled (`FetchHistory`, at most 20 000) and the screen comes from this client's pane buffer; each row whose text matches the archive read is written with SGR sequences (reset at the end of every line, control characters dropped), every other row (archived history, rows that changed meanwhile) as plain text, so the file has exactly the viewer's lines and `+N` still lands on the viewer's line. A server that doesn't report `mem_first` gets the plain file with a note. *Batch 2B:* on the local machine the editor now runs in a 90%×90% popup pane over the TUI (`pane.float {popup}`), and the temp file is deleted when the popup is gone; for remote panes the editor still runs on the host terminal with the TUI suspended, because the remote server can't read this client's temp file. Copy-mode keys are configurable (`[keys.copy_mode]`, see §14 below); mouse selection and copy-on-select see §14 |
| Config reload | §11.2 | M1 |
| Socket API + CLI | 07 | M1 |
| `agent start / prompt --wait / wait / read / send-keys` | 04, 07 | M1 |
| Built-in integrations | `vibeke integration install` (Claude, pi/omp, Codex) | M1; others M2 |
| Agent self-report (`pane report-agent`) | adapter API + compat | M2 |
| Agent skill | `vibeke --skill` | M1 |
| Agent resume after restart | 04 resume handles | M1 |
| Worktree helpers | git worktree tasks (05) | M1 |
| Remote via SSH, saved machines, `--machine` forwarding | 06 | M3 |
| Remote image paste | 06 | M3 |
| Layout export/apply | 07 `layout.*` | M4 — built (API + CLI, named layouts in `[layouts.<name>]`). TUI (`vk-tui::layouts`): palette `layout_save` asks for a name, runs `layout.export {tab, format: toml}` and — there is no config-write API — shows the `[layouts.<name>]` snippet (headers re-rooted) and copies it to the clipboard; `layout_apply` lists `layout.list` (invalid ones shown with their error and refused) and applies with `enter` (new workspace) or `w` (into the focused workspace) |
| Plugins (full Herdr plugin contract + native process/UI/storage additions) | 07 | M5; Windows M6; marketplace post-1.0 |
| Live handoff on update | normal path via holders | M1 |
| Update channels | §11 `[update]` | M1 |
| Windows host | — | M6 |

## 14. Requested features we ship

| Request | What we ship | Milestone |
|---|---|---|
| Multiple remote servers in one client | Machines tree, combined attention across machines, `--machine` on every CLI verb (06) | M3 |
| Tab numbers with custom titles | `ui.tabs.show_numbers` | M1 |
| Same session in several terminal windows | Multi-client by design; independent viewport and focus per client | M1 |
| Mark unread | `marked_unread`, `prefix+u` | M1 |
| `/` search in copy mode | in-buffer vi search (M1); FTS over archived scrollback (M4) | M1 / M4 |
| Async worktree delete | background removal job with progress; UI never blocks (05) | M1 |
| Hierarchical groups | `Group` entity, aggregate badges | M4 — built: model/API (`group.*`, `SessionModel.groups`) and the sidebar group level with badges, collapse, move keys/picker and drag-into-group (§2.1) |
| Copy-on-select (PRIMARY) | `clipboard.copy_on_select` (default `true`), `primary_selection`, `mouse_select_in_apps` | M4 — built (`vk-tui::selection`, `vk-tui::copyout`; 03 §11.1): a left drag in a pane without mouse reporting — or over one with it, `shift`+drag or `alt`/`option`+drag (`mouse_select_in_apps = "modifier"`), or any drag with `"always"` (the app then gets clicks, sent on release, and the wheel) — enters copy mode with a highlighted character selection from the press to the pointer; a double click selects a word, a triple click the logical line; dragging past the top/bottom edge autoscrolls, the wheel scrolls during a drag, and drags outside the pane keep extending. On release `copy_on_select = true` copies it (soft wraps joined, trailing blanks trimmed) with a `copied N chars` / `copy failed — why` toast and leaves copy mode; otherwise copy mode stays with the selection for `y`. A plain click does nothing new; inside copy mode a press restarts the selection, a drag extends it, a click clears it and the wheel scrolls (loading older rows at the top). The scrollback viewer selects the same way. Copies go out as OSC 52 (tmux DCS passthrough under `TMUX`, not above `remote_write_max_bytes`) plus `pbcopy`/`wl-copy`/`xclip`/`xsel` when not over SSH; iTerm2 over SSH gets a one-time hint about its "Applications in terminal may access clipboard" setting. Every user copy also sets PRIMARY when `primary_selection = true` (OSC 52 `p`, plus `wl-copy --primary` / `xclip -selection primary` / `xsel -p` locally; none on macOS), and a middle click then pastes the last copy into the pane. Ghostty keeps `shift`+drag and iTerm2 `option`+drag for their own selection, so `alt` works in Ghostty and `shift` in iTerm2 |
| Configurable copy-mode keys | `[keys.copy_mode]` | M4 — built (`vk-tui::copykeys`, `vk_config::COPY_MODE_ACTIONS`): `mode = "vi"` (the M1 keys) or `"emacs"` (`ctrl+f/b/n/p`, `ctrl+a/e`, `alt+f/b`, `alt+<`/`alt+>`, `ctrl+v`/`alt+v`, `ctrl+space` mark, `alt+l` line, `alt+r` block, `ctrl+s`/`ctrl+r` search, `n`/`N`, `alt+w`/`enter` copy, `ctrl+g`/`esc` cancel, `q` exit) as the base table; per-key overrides `key = "action"` replace the base entry for that key, `""` unbinds it. Actions: `exit cancel left right up down half_page_up half_page_down page_up page_down line_start line_end top bottom view_top view_middle view_bottom word_next word_prev word_end select_char select_line select_block search_forward search_backward search_next search_prev copy edit_scrollback prompt_prev prompt_next select_output` (the last three are the OSC 133 actions of 03 §8, bound to `[`, `]`, `o` in vi and `alt+{`, `alt+}`, `alt+o` in emacs). A key that isn't a single plain key or an unknown action is a config warning and is ignored. Applied on config reload |
| Jujutsu workspaces | — | Dropped: jj support was removed for v1 (2026-10-06); git worktrees only. |
| Tab bar at the bottom | `ui.tabs.position = "bottom"` | M4 — built (§3 as built; also `"hidden"`) |
| Status bar | built-in segments (M4); plugin segments (M5) | M4 / M5 — built-in segments built (`status.segments` + the TUI bar, §4); plugin segments M5 |
| Sidebar left/right | `ui.sidebar.position` | M4 — built (§2.4 as built) |
| Floating panes | floating panes + popups (popups for custom commands in M1) | M4 — built: model/API (`Tab.floating`, `pane.float/embed`, `tab.floats`) and TUI drawing, mouse/keys and `ViewHint` sizing (§5) |
| Click notification to focus | native notifier with `vibeke://focus` | M4 — `vibeke focus <url>` + notifiers built (§7.1 notes) |
| Command palette | `prefix+:` / `ctrl+shift+p` | M4 |
| Mosh transport | QUIC roaming + predictive echo (06) | post-1.0 |
| Synchronized input | `prefix+shift+s`, agents excluded by default | post-1.0 |
