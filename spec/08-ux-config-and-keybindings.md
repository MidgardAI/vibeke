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

## 3. Tab bar

- Position `ui.tabs.position = "top" | "bottom" | "hidden"`.
- Each tab shows `number` plus title: `3 review`. `ui.tabs.show_numbers = true`. Numbers are per workspace, assigned at creation, and renumbered only on explicit `tab renumber`.
- Title source order: custom title → `auto_title` (the focused pane's agent name or harness, else the process name, else OSC title) → `shell`.
- The tab shows the most urgent agent state among its panes as a glyph prefix (`⚑`, `?`, `✓`).
- Overflow: scroll arrows plus the `prefix+g` goto. Mouse: click to switch, middle-click to close (confirm if processes are running), drag to reorder.

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

*Implementation (M4, server side):* `status.segments {pane?, client?}` (07 §2.1) returns the data for every built-in segment from a client's focus: machine, session, workspace, task, branch, ports with live previews, attention (count plus oldest unfocused interaction), agents_summary (working/done/needs_input/error), cpu load, clock and sync_input. `mode` and `prefix_indicator` are client state. Open TUI work: draw the bar from `ui.status_bar`, refresh on model changes plus a 1 s clock tick, and make the `attention` click open the oldest card.

**Plugin segments** (M5): plugins contribute `status.segment` with `{id, interval_ms | event_driven, render → spans}` (07). They appear as `plugin:<id>/<segment>` in the lists above. A segment that renders slower than 50 ms is dropped with a warning.

## 5. Panes, floating panes, popups, zoom, resize

- **Splits**: `prefix+v` (vertical, side by side), `prefix+minus` (horizontal). `pane.split` via API supports `--size 30%`.
- **Zoom**: `prefix+z` toggles the focused pane to fill the tab. A `Z` marker appears in the tab.
- **Resize mode**: `prefix+r` then `h j k l` / arrows (shift = ×5), `=` equalizes, `esc`/`enter` exits. Mouse: drag borders.
- **Floating panes** (M4): `prefix+f` creates a floating pane (default 70%×70%, centred); `prefix+shift+f` toggles visibility of all floats in the tab. Floats can be moved/resized with the mouse or in resize mode (`m` toggles move). A tiled pane can be floated and back (`pane float`/`pane embed`).
  - *Implementation (M4, server side):* `Tab.floating: [{pane, x, y, w, h, z}]` (percent of the pane area) and `Tab.floats_hidden`; `pane.float`, `pane.embed`, `tab.floats` (07 §2.6). Floats survive layout export/apply; closing a tab's last tiled pane closes its floats. Open TUI work: draw floats over the tiling (z order, border, focus ring), skip them when `floats_hidden`, `prefix+f`/`prefix+shift+f`, mouse move/resize and resize-mode `m`, and include floats in `ViewHint` so their PTYs get real sizes (they stay 80×24 until then).
- **Popups** (`type = "popup"`): session-modal terminals that don't change the layout and close when the command exits (used by `[[keys.command]]`, edit-scrollback, plugin actions). Width and height accept cells or `%`.
- **Synchronized input** (post-1.0): `prefix+shift+s` toggles sync for the current tab. Input to the focused pane is mirrored to every pane in the tab that is in the sync set (default all; `prefix+alt+s` toggles a single pane's membership). A bright `SYNC` badge shows in the tab bar and the status bar. Paste is mirrored too. Agent panes are **excluded by default** (`ui.sync_input.include_agents = false`) to avoid prompting N agents by accident.
- **Focus follows mouse**: `ui.focus_follows_mouse = false`. When enabled, hovering a pane focuses it after `ui.focus_follows_mouse_delay_ms = 120`; it never applies while a popup or card is open.
- **Close**: `prefix+x` closes the pane after confirmation when a non-shell foreground process is running (configurable `ui.confirm_close = "running" | "always" | "never"`).

## 6. Navigation

### 6.1 Navigate mode
`prefix+w` opens navigate mode: the sidebar gets keyboard focus. Movement: `up/down` move between workspaces and agents, `h j k l` move between panes, `enter` focuses, `1..9` jump, `esc` exits. Plus: `/` filter, `space` peek (§6.4), `a` answer (opens the interaction card for the selected agent, if unfocused), `u` mark unread, `p` pin, `r` rename, `x` close (confirm), `n` new workspace, `t` new task (05).

### 6.2 Goto / fuzzy switcher
`prefix+g`: one fuzzy list over workspaces, tabs, panes, agents (by name, harness, state), tasks, previews and machines. Typing filters; prefix tokens narrow the kind: `@agent`, `#task`, `:tab`, `>command` (switches to the palette), `!state` (`!approve` lists everything awaiting approval). Results are ranked by match score, then recency, then urgency. `enter` jumps; `ctrl+enter` opens in a new client split view (M4).

*As built (research R5):* `vk-tui::nav`. Candidates: workspaces (`~`), tabs (`:`), panes/agents (`@` for panes with a run) and tasks (`#`; untracked tasks only when asked for). Matching is a real fuzzy matcher (fzf-style subsequence with word-boundary/camelCase/first-char bonuses, consecutive-run bonus, gap penalties; every whitespace token must match) over the shown label **plus hidden fields**: workspace root/repo path and git branch, tab/pane handles, pane cwd, the agent's harness, model, run handle and **native session id** (`harness_session_id`), task repo/branch/worktree. Matched letters are highlighted (bold+underline) in the label. Ranking: score, then this client's **recent targets** (per-client history, §6.7), then urgency (open interaction, question, done), then natural order; with an empty query recent targets come first, then urgent ones. `!state` filters by run state (`!approve` = open interactions), kind prefixes narrow, `>` as the first key switches to the palette. Previews and machines are not candidates yet.

### 6.3 Command palette
`prefix+p` is taken by `previous_tab`, so the palette is **`prefix+:`** and also `ctrl+shift+p` as a direct binding when the host reports it unambiguously (kitty keyboard). It lists every action (built-in, `[[keys.command]]`, plugin actions) with its current binding. Actions take arguments through inline prompts (for example `split: size?`). It remembers the last 20 used.

*As built (research R5):* `Popup::Palette` (`prefix+:`; the `:` text prompt it replaced is gone). Entries: every `DEFAULT_KEYMAP` action with a one-line description and its effective binding, the browser-pane context actions (binding shown as `… (browser pane)`), palette-only TUI commands (`track_work`, `task_details`, `pending_operations`, `open_preview`, `browser_stop`, `browser_watch`), `[[keys.command]]` entries (`Command: <title>`), and **live entries** `Watch agent browser b3 — <url>` per agent browser session on each connected machine (listed with `browser.list` when the palette opens; refreshed by pushed `browser.*` events). Fuzzy-matched against "description  action_name" with highlights; recently used entries (last 20, per client, persisted) come first with an empty filter and get a bonus otherwise. `↑/↓`, `tab`, `ctrl+n/p` move, `ctrl+u` clears, `enter` runs, `esc` closes. No inline argument prompts yet; plugin actions don't exist yet.

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

### 6.6 Fleet grid and inbox views
- **Fleet grid** (`prefix+F`, M4; a view, not a layout change): one tile per agent run. **Tiles default to a live miniature of the real terminal** (the agent's own UI, scaled by cropping to the last rows); `t` toggles a tile (or `T` all tiles) to Vibeke's structured timeline (tool calls, diff size, state, preview). `ui.fleet.tile_view = "terminal" | "timeline"`. `enter` focuses the real pane.
- **Inbox view** (`prefix+i`, M1): a full-screen list of open interactions on unfocused agents, ranked by urgency (risk, wait, blocking), with the card for the selected item and a peek of the agent. It replaces the pane area while open (agents keep running; nothing is drawn over a focused pane because there is none while the view is open). `esc` returns.
  - *Evolved in place for spec 15 T3 (implemented):* the same `prefix+i` view consumes `attention.list` from every connected machine and merges it (coverage lines for offline machines or incomplete coverage; offline items shown as last observed with actions disabled). Items: interactions, review available / check failed / send unknown, suspended bindings, and a "Finished turns" footer. Each row has a deterministic explanation ("Waiting 12m · blocks this run"). Keys: `j/k`, `enter` (card or task details), `y/n/1-9` quick answers, `o` open pane (the only focus change), `s` snooze (15m / 1h / tomorrow 9:00 / custom), `e` effort, `f` five-minute view (urgent items always shown, omitted count visible). Selection is stable by item key while the list reorders; newly urgent items get a ● marker instead of stealing selection. Falls back to the M1 ordering when the server lacks `attention.list`. `prefix+a` follows the ranking.
  - **Task details** (explicitly opened full pane-area view, never automatic): from the inbox, goto (`#k1`), agent peek `t`, or `:task_details`. Shows ownership/lifecycle, review label, intent with the verbatim source request, "Agent has not been told about revision N", bindings (`c` Continue task / `n` Track new work), messages with delivery state, requirements, observed commands (agent prose shown as *claim*), checks; actions `e` edit intent, `w` message agent (prepare → exact text + recipient → explicit send; refused sends offer Open pane), `v` run check (first run asks "Authorize for this revision"), `m` mark reviewed (reason per unsupported required criterion), `o` open pane.
  - **Track this work**: agent peek `t` or `:track_work` → form with the selected request verbatim (↑/↓ other turns), editable title, optional criteria, stop-at starting at *Not specified*, objective; `ctrl+s` tracks. Unverified runs show "Link run first". Sidebar agent rows show the review label as a separate small marker (`◆review`) beside `✓ done`.
  - Pending task mutations persist in `<state>/<session>/client-pending.json` before dispatch and are reconciled via `task.operation.get` on reconnect; unknown outcomes are never resubmitted automatically (`:pending_operations`).

### 6.7 Session desk and drafts composer (research R2/R3) — planned, not yet built

The server/CLI side exists (07 §2.14a: `desk.*`, `draft.*`, `notes.*`); these TUI surfaces are **not built**. No new default keybindings: entries live under the palette, goto and agent peek.

- **Session desk** (`:desk`, goto prefix `?` for conversations): a full pane-area view like the inbox. A query line (`desk.search`), filter chips for repository (default: the current workspace's repo), harness and date (`7d`, `30d`, custom), and result rows `harness · repo · session · turn n · age · snippet` with a status marker (`● live`, `↻ resumable`, `· none`). `enter` opens the result detail (`desk.open` with `turn`: the turn's items rendered like the peek body). Actions, labelled exactly: `o` **Focus live pane** (live only; `desk.open focus=true`), `r` **Resume native session** (shows the exact command and asks for a target: a new tab in the session's workspace or a chosen free pane), `c` **Start new agent with context** (opens the `desk.context` package in an editor buffer with selectable turns; saving creates a draft; optional "start harness" toggle; never sends). A sessions tab lists `desk.sessions`. A footer shows coverage from `desk.status` (sources, opted-in roots, exclusions, retention, pending bytes) and `F` forget (confirm, `desk.forget`).
- **Drafts composer** (`:drafts`, agent peek `d`): a side panel per workspace (and per task in task details) listing drafts in order with attachment counts and the last send state (`sending`, `✓ delivered`, `? unknown`, `✗ failed`). Keys: `n` new, `e` edit (multi-line editor; `ctrl+f` attach a file, `ctrl+s` attach the latest screenshot/clipboard image via `blob.put`), `J/K` reorder (`draft.reorder`), `space` select, `m` combine selected, `x` delete (confirm), `s` send → target picker defaulting to the run in the focused pane, showing `draft.check` (`send_path: prompt_input`, or "Open pane to send" with the reason, plus "steer: not supported") and an "include notes" toggle; refused sends keep the draft and offer `o` open pane. A `? unknown` draft offers `R` reconcile (`draft.reconcile`) before a warned retry. Pending sends persist their idempotency key like task mutations (`client-pending.json`).
- **Notes** (`:notes`, a tab in the drafts panel): one plain-text document per workspace (`notes.get/set`, `expected_rev` conflict prompt), labelled "Never sent unless you include it".
- The agent peek reply box (§6.4) gains "Save as draft" (`ctrl+d`) so a half-written follow-up never has to live in the harness's own input.

### 6.8 Recent targets, last workspace, hints and the terminal title (as built, research R5)

- **Per-client history** (`vk-tui::nav`): `<state>/<session>/nav-<client>.json` where `<client>` is `$VIBEKE_CLIENT_NAME` (sanitized) or `default` — so two laptops, or two named clients on one machine, keep separate histories. It holds the last 50 focused targets (machine label + pane/task id + workspace), the last 20 palette actions, and the current and previous workspace. Focus changes are observed once per frame.
- **`last_workspace`** (default `prefix+shift+l`; spec 08 had no binding and `prefix+l` is `focus_pane_right`) toggles to the previously focused workspace (across machines), landing on the pane this client last used there, else the workspace's first tab.
- **Hints** (`url_hints`, default `prefix+shift+u`; `prefix+u` stays `mark_unread`): labels (`a s d f j k l g h …`, two letters past 26) over every URL, `file:line[:col]` path, git SHA (7–40 lower-case hex with a digit and a letter), Vibeke handle (`w2:p1`, `b3`, `v4`, `#k12`, …) and ULID visible in the focused pane. A label **opens** URLs — loopback URLs as a browser pane next to the pane (that machine's `localhost`, window without graphics), other `http(s)` URLs with the local OS opener (`open`/`xdg-open`) — and **copies** everything else (OSC 52 / OS clipboard); `SHIFT+label` always copies. Any other key closes. `file:line` targets are not verified against the file system yet.
- **Terminal title sync**: `ui.title_sync = true` writes OSC 2 with `ui.title_format` (default `"{workspace} · {pane}"`; also `{tab}`, `{machine}`, `{session}`; the pane part is the agent's name/harness, else the pane title) whenever it changes; the host's own title is pushed (`CSI 22;2t`) before the first write and popped (`CSI 23;2t`) on exit. Control characters are stripped, 120 chars max.

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
- **Open TUI work:**
  - send `host: {bundle_id: $__CFBundleIdentifier, term_program: $TERM_PROGRAM}` in `render.attach`;
  - skip the OSC forward when `Notify.delivered` contains `native`;
  - drop the TUI's own presence check for frames the server already filtered;
  - show coalesced counts on toasts.

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

Action names: the copy-mode binding is `enter_copy_mode` and the picker is `workspace_picker`, because `[keys.copy_mode]`/`[keys.navigate]` are tables (Goal 01 deviation).

**Previews and browser panes (06 B2/B3.2; Goal 03 Stage 2).** `prefix+o` (`open_notification_target`) keeps its usual meaning while a toast with a target is showing; otherwise it opens the focused pane's preview as a browser pane next to it (a window when the host has no graphics); `open_preview` is the same action without the toast rule. While a **browser pane is focused**, a browser table is consulted before the global one; it overrides keys that mean nothing in a browser pane (copy mode, scrollback editing, sync input). Rebind with `[keys] browser_back = "…"` etc.

| Action (browser pane focused) | Default | Overrides |
|---|---|---|
| ✚ browser_address (edit URL) | `prefix+e` | edit_scrollback |
| ✚ browser_back / browser_forward | `prefix+[` / `prefix+]` | enter_copy_mode / paste_buffer |
| ✚ browser_reload / browser_hard_reload | `prefix+.` / `prefix+,` | — |
| ✚ browser_screenshot | `prefix+shift+s` | sync_input |
| ✚ browser_window (open in window ⇄ back to pane, 06 B3.3) | `prefix+o` | open_notification_target |
| ✚ browser_console (console/network split) | `prefix+alt+c` | — (not built yet: Stage 3) |
| ✚ browser_take_over (watch pane: take over ⇄ release the agent session, 06 B7) | `prefix+t` | — (unbound globally) |

All other keys go to the page except the prefix; direct (non-prefix) bindings such as `ctrl+v` don't apply in a browser pane. Mouse: click the chrome's ←/→/⟳ or its URL; Ctrl/Alt+click a `http://localhost:<port>` URL printed in any pane opens it in a browser pane next to that pane.

\* `rename_pane` is bound to `prefix+shift+p`, so we default `pin_pane` to `prefix+alt+p` (§2.3 references to "pin" use this binding). `vibeke keys check` must report no conflicts on the shipped defaults (CI test).

Mode-local keymaps: `[keys.navigate]`, `[keys.copy_mode]`, `[keys.resize]`, `[keys.card]`. All are rebindable.

### 10.3 Custom commands
`[[keys.command]]` (`type = "shell" | "pane" | "popup"`, `width`/`height`) plus ✚ `type = "float"` (persistent floating pane), ✚ `cwd = "pane" | "workspace" | path`, ✚ `env`, ✚ `title`, and ✚ `when = "agent:claude"` (only active when the focused pane runs that harness).

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
[terminal.env]                        # extra env injected in every pane
EDITOR = "nvim"

[clipboard]
osc52_write       = "allow"           # allow | deny
osc52_read        = "deny"            # deny | ask | allow
copy_on_select    = false
primary_selection = false
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
vcs               = "auto"            # auto | git | jj (jj: M4) — VCS detection
checkout          = "auto"            # auto (jj workspace if .jj, else worktree) | worktree | jj | clone | none (05 §4; clone is the default for container/vm, 13 §6)
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
tls_origin        = false             # proxy mode: serve https://*.vibeke.localhost with a local CA
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
| Native notifications, click-to-focus | §7 | M4 — server pipeline, notifiers and `vibeke focus` built; TUI `host` report + helper bundle open |
| Keybindings with prefix, custom commands (shell/pane/popup) | §10 | M1 |
| Themes | §11 | M1 (fixed), M4 (auto light/dark + propagation) — server side built (`theme.mode`, `client.appearance`, `theme.changed`, `SessionModel.appearance`, `COLORFGBG`/`VIBEKE_THEME` in new panes); open TUI work: query OSC 11 / subscribe `CSI ? 2031 h` + `CSI ? 996 n` and report it, switch `dark_name`/`light_name` on `appearance` changes, answer panes' OSC 10/11 queries with the effective palette |
| Copy mode | 03 §11 | M1 (vi keys, `/` in buffer), M4 (archive search, edit scrollback) — server side built (`search.query`, `pane.read {source: archive}` paging by absolute line); open TUI work: copy-mode `/` falling back to `search.query` for the pane, paging older rows via `pane.read archive` instead of `FetchHistory` past memory, edit-scrollback popup writing the archive range to a temp file for `$EDITOR`, a search popup over `search.query` |
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
| Layout export/apply | 07 `layout.*` | M4 — built (API + CLI, named layouts in `[layouts.<name>]`); open TUI work: palette entries "save layout"/"apply layout" |
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
| D#1620 Hierarchical groups | `Group` entity, aggregate badges | M4 — model/API built (`group.*`, `SessionModel.groups`, aggregate `agent_summary`); open TUI work: group level in the sidebar tree with collapse, drag-into-group, badges |
| D#748 Copy-on-select (PRIMARY) | `clipboard.copy_on_select`, `primary_selection` | M4 |
| D#587 Configurable copy-mode keys | `[keys.copy_mode]` | M4 |
| D#480 / D#2209 Jujutsu workspaces | `tasks.vcs = "jj"` (05) | M4 — built (`task new --isolation jj_workspace`, auto-detected); open TUI work: show bookmark/change instead of branch for jj tasks |
| D#834 Tab bar at the bottom | `ui.tabs.position = "bottom"` | M4 |
| D#1629 Status bar | built-in segments (M4); plugin segments (M5) | M4 / M5 — segment data API built (`status.segments`); drawing is TUI work |
| D#1465 Sidebar left/right | `ui.sidebar.position` | M4 |
| D#782 Floating panes | floating panes + popups (popups for custom commands in M1) | M4 — model/API built (`Tab.floating`, `pane.float/embed`, `tab.floats`); drawing/input is TUI work (§5) |
| Click notification to focus | native notifier with `vibeke://focus` | M4 — `vibeke focus <url>` + notifiers built (§7.1 notes) |
| Command palette | `prefix+:` / `ctrl+shift+p` | M4 |
| Mosh transport | QUIC roaming + predictive echo (06) | post-1.0 |
| Synchronized input | `prefix+shift+s`, agents excluded by default | post-1.0 |
