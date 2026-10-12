# 19 — Mentions: names and messages between agents

Status: phase 1 in progress.

## 1. Goal

You give a pane a short name, such as `@reviewer`. Then you, or an agent in another pane, can
send that pane a message by its name. The reply comes back to the pane that asked. Messages
stay inside one workspace on one machine.

Phase 1 covers names, the `prefix+@` picker, direct messages and replies for Claude Code and
Codex, MCP tools, CLI verbs and web composer completion. Groups, terminal targets, a Messages
view, tracked coordination and pinned context come later (§9). Personas are left out.

## 2. Names

### 2.1 Rules

- A mention name matches `[a-z][a-z0-9_-]{0,31}`, does not end with `-` or `_`, and is not one
  of `vk_proto::mention::RESERVED` (`current`, `focused`, `all`…). `vk_proto::mention::valid`
  is the one check. A `-` or `_` right after `@name` is punctuation, so `@reviewer_` means
  `@reviewer`.
- A name belongs to one pane (`MentionName.pane`) and is unique in its workspace. It stays
  when the agent in the pane exits or restarts. It goes away with the pane.
- A pane has at most one name. `SessionModel.mentions` lists all names (render protocol 10),
  so every client can draw them.
- `kind` is `agent` or `terminal`. Phase 1 delivers only to `agent` panes.
- `delivery: false` keeps the name but holds new deliveries.
- A name is frozen once given. It does not follow later label changes.

### 2.2 Suggested names

A pane without a name gets a proposal from `vk_proto::mention::suggest(label, fallback,
taken)`. `label` is the sidebar label (`AgentRun::label()` for an agent pane, otherwise
`Pane::display_title()`). `fallback` is the harness for an agent pane, otherwise the base name
of the pane's cwd. Taken names get `-2`, `-3`….

### 2.3 Consent

Only named panes receive messages. A person names a pane in the TUI picker (§5), the palette,
the CLI or the web app. An agent may name only its own pane. Naming a pane is the consent for
it to receive messages from other panes of the workspace.

## 3. Data and API

All writes go through `Core::commit`. Names live in `SessionModel.mentions` and the store.
Messages and deliveries are store entities, not part of the render model. Events carry ids and
states, never bodies.

### 3.1 Methods

All methods are additive to `vibeke/1`. "Pane caller" means a caller with a pane token.

| Method | Params | Result | Pane scope |
|---|---|---|---|
| `mention.list` | `workspace?` (default: the caller's workspace, else all) | `{mentions: [MentionName + label, harness?, state]}`, where `state` is `idle`, `busy`, `needs_input`, `exited` or `shell` | open |
| `mention.suggest` | `pane` | `{name}` | open |
| `mention.set` | `pane`, `name?` (omitted: suggested), `kind?`, `delivery?` | `MentionName` | own pane only |
| `mention.clear` | `pane` | `{}` | own pane only |
| `message.send` | `to: [name]`, `body`, `expects_reply?` (default true), `workspace?` | `{message, deliveries: [{id, name, state}]}` | open, own workspace only |
| `message.reply` | `message` (the one being answered), `body` | `{message, deliveries}` | the recipient pane of that message |
| `message.complete` | `message` | `{}`: the caller's delivery becomes `completed` | the recipient pane |
| `message.get` | `id` | `{message, deliveries}` | sender or recipient pane |
| `message.list` | `pane?`, `direction?` (`in`, `out`, `all`), `open_only?`, `limit?` | `{items: [{message, deliveries}]}` | own pane only |
| `message.cancel` | `delivery` | `{}` | the sender; people always |
| `message.retry` | `delivery` | `{}` | people only (`PANE_FORBIDDEN`) |

Errors:
- `invalid_params`: a bad name, an empty body, or a body over 64 KiB.
- `conflict` with `name_taken`.
- `not_found`: an unknown recipient. The error data lists the unknown names and the known
  names.
- `permission_denied`: another workspace, or a recipient with `kind: terminal` in phase 1.
- `conflict` with `hop_limit`: `hop` would pass `MAX_HOPS`.
- `rate_limited`: more than 20 messages a minute from one pane.

Sending to yourself is `invalid_params`.

Events: `mention.set`, `mention.cleared`, `message.sent`, `message.delivery` (with `delivery`,
`message`, `name`, `state`, `reason?`).

### 3.2 Targets

`@name` is a target (07 §1.2). It resolves to the named pane in the caller's workspace. Outside
a pane it resolves across workspaces only when the name is unique, otherwise
`ambiguous_target`. `@current` and `@focused` keep their meaning, which is why those words are
reserved. `agent.spawn`/`agent.start` with a `name` also set the pane's mention name when it is
valid and free.

## 4. Delivery

### 4.1 Envelope

The server never types a message body into a pane. When the recipient is ready (§4.2), it types
one line with `agent.prompt` semantics (dialog guard, stall detection):

```
↪ vibeke message <message-id> from @coder · <first line of the body, at most 60 chars> [<nonce>]
```

The nonce is 12 random base32 characters, stored on the delivery and never returned by the API.
The delivery becomes `submitted`. The nonce works once: the hook that expands it moves the
delivery to `delivered` in the same step, and `message.retry` gives the delivery a new nonce.
Everything except the mention handler observes the typed line without its nonce and preview
(`↪ vibeke message <id> from @coder · … […]`), so items, turns and events never carry them.
Reads of the pane and the agent do the same for every caller: `pane.read`, `agent.read`,
`agent.transcript` and the gateway transcript replace the preview and nonce with `… […]`. The
match allows soft wraps anywhere in the line, so a nonce that wraps onto the next row is
replaced too. Styled reads (ANSI) of a screen that shows an envelope fall back to plain text.

Titles derived from the line are scrubbed as well. A harness may name its session after the
envelope (Codex named a thread `Answer 2+2 [EQ4B36KPSJ6A]`). So the run title
(`AgentRun.title`, from transcripts and Codex thread names) and the pane's terminal title in
the render stream and the compat API go through `scrub::scrub_title`: an envelope becomes
`Message from @coder`, and a nonce bracket is removed anywhere. The preview a harness reworded
stays; the nonce does not. `AgentRun.last_message` is scrubbed like a pane read. Vibeke cannot
change what the harness draws in its own status bar.

### 4.2 When to deliver

A delivery is ready when all of these hold:
- The recipient pane has an agent run in `idle` with no open interaction.
- The agent's input box is provably empty, so no draft of the person's is sent along. Vibeke
  reads the box from the styled screen. For Claude it is the `❯` (or `>`) line below a
  horizontal rule (or inside a rounded box), up to the next rule. For Codex it is the lowest
  `›` (or `>`) line and the lines below it, up to the first blank line. The box is empty when
  every visible character after the prompt marker is drawn like a placeholder: faint, or in a
  gray color, on the first line only (the cursor may sit on its first character). Typed text,
  any text on a later line, and pasted-text chips (`[Pasted text …]`) are a draft. When this
  layout is not found, the plain-text reader decides: the box is empty only when nothing
  follows the prompt marker. For harnesses other than Claude and Codex, an input box Vibeke
  cannot read counts as empty.
- When a TUI client focuses the pane, no input reached the pane in the last 5 s.
- No other delivery to that pane is `submitted`.
- The name has `delivery: true`.

Otherwise it stays `queued`. The input check runs again right before typing. From that check
until the Enter is written, keys, pastes and mouse input that people send to the pane wait in a
buffer, and reach the pane right after the Enter, in order. The hold lasts at most 2 s.
`agent.prompt` holds input the same way between its paste and its Enter. The engine re-checks when run state, interactions or names change,
and once per second while anything is queued.

A delivery is `held` with a reason in these cases:
- The sender or the recipient moved to another workspace (`moved_workspace`). This is also
  checked when the hook expands an envelope, and `message.reply` refuses it.
- The pane has no agent.
- The agent exited.
- The prompt was refused twice.
- The delivery stayed `queued` for 24 hours.

### 4.3 Hooks

The `vibeke hook` shim already prints a `hook_output` from the `adapter.signal` reply. The
mention handler adds to that reply for Claude and Codex. It merges `additionalContext` with the
collision tracker's output.

**`UserPromptSubmit` with an envelope.** The handler finds the delivery by message id and
compares the nonce.
- On a match, the delivery becomes `delivered`. The handler adds the full message as
  `additionalContext`:
  - framed as a message from another agent, untrusted, granting no permissions;
  - with the sender's name, the message id and the thread;
  - with how to answer (§6) when `expects_reply`;
  - with the roster (§4.4).
- When the nonce does not match, the context says the line is not an authentic vibeke message.

**`UserPromptSubmit` from a person** whose prompt starts with mentions
(`vk_proto::mention::parse(prompt).leading`):
- When every leading name is known in the workspace and deliverable, the server sends the rest of
  the prompt as a `Human` message from this pane, with `expects_reply: true`. The hook blocks the
  local prompt with the reason `Sent to @reviewer (m_…). The reply will show up here.`
  When the prompt came through `agent.prompt` (the web composer, a TUI reply), that call returns
  `mention_sent: <message id>` instead of failing with `agent_prompt_stalled`.
- Claude Code and Codex put a blocked prompt back into the input box. That draft would keep
  every later delivery to the pane queued (§4.2), the reply included. So after the handoff,
  Vibeke watches the input box for 3 s. When the box shows exactly the routed prompt
  (whitespace aside), it takes the pane's input hold and clears the box with the line-kill key
  (ctrl+u; a backspace when a key changed nothing, for a line break), at most 20 keys, reading
  the box again after each. Any other text is the person's and stays.
- When any leading name is unknown, nothing is sent. The prompt goes through with a short note
  that lists the known names.

**`UserPromptSubmit` with embedded mentions** of known names adds the roster and a one-line
reminder that the agent can delegate with the `message_send` tool.

**`SessionStart`** adds the roster when the workspace has named panes, plus the pane's own name
when it has one.

**`Stop`** for a pane with a `delivered` delivery that expects a reply and has none:
- The first time, the hook blocks the stop with one reminder to reply or complete
  (`{"decision": "block", "reason": …}`).
- The second time, the delivery becomes `completed` without a reply.

**Headless runs** have no hooks. They get the framed full body as their prompt right away, and
the delivery becomes `delivered`.

**No hook.** A `submitted` delivery that no hook took within 30 s:
- For harnesses without mention hooks, the engine falls back once to typing the framed full
  body. The fallback waits, still `submitted`, until the agent is idle and its input box is
  safe (§4.2). Only a fallback that was typed counts as tried.
- Otherwise the delivery becomes `held` with the reason `hooks_missing`. `vibeke doctor` explains
  how to install the integration.

**Turn end without a `Stop` hook.** A delivered message records the recipient's run and its
turn counter (`done_rev`). For headless runs and harnesses without mention hooks, the delivery
becomes `completed` when that turn ends (the counter moves on), or when the run is gone or
replaced. A message that expected an answer and got none completes with the reason `no_reply`.

### 4.4 Roster

The roster is one line per named pane of the workspace:
`@name · agent|terminal · harness · idle|busy|needs input|exited`. The caller's own name is
marked `(you)`. It never includes message bodies.

### 4.5 Replies

`message.reply` creates a message with `in_reply_to` and `hop + 1`. It goes to the original
author:
- **An agent author** gets it through §4.1 with `expects_reply: false`.
- **A `Human` author with a pane** (a leading mention) also gets it through §4.1 into that pane.
  The person sees the answer where they asked.
- **A `Human` author without a pane** (TUI, web) gets a notification. The reply stays in
  `message.list`.

The answered delivery becomes `replied`. Several recipients reply one by one in phase 1.

### 4.6 Safety

- Messages stay within a workspace.
- Bodies are redacted with `vk-redact` before they are stored.
- Bodies are labelled as untrusted in every context the server writes.
- The hop limit, the rate limit and the 64 KiB body cap apply. A pane passes on the hop of a
  message only while it works on it: in the turn the message was delivered in.
- `message.retry` and naming other panes are for people only.

## 5. TUI

**`prefix+@` (action `mention_picker`)** opens the Mention picker next to the focused pane's
cursor. It lists the other panes of the workspace (no browser panes or plugin surfaces). Each
row shows:
- the name, or the suggested name in grey with `→`;
- the sidebar label;
- the state (`idle`, `busy`, `shell`).

The search is fuzzy over name, label and harness. The keys are:
- **Enter** inserts `@name ` into the focused pane as a paste and closes the picker. For an
  unnamed pane it first calls `mention.set` with the shown name.
- **Tab** inserts the name and keeps the picker open.
- **Ctrl-E** edits the shown name in place.
- **Esc** closes the picker.

Named panes show `@name` in the pane border and the sidebar. The palette offers **Mention a
pane** (the picker), **Name this pane** and **Remove this pane's name**.

When a reply reaches a `Human` author without a pane, a toast appears.

## 6. Agent tools

**MCP.** `vibeke mcp` adds these tools for agents in a pane:
- `mentions_roster`
- `message_send {to, body, expects_reply?}`
- `message_reply {message, body}`
- `message_complete {message}`
- `message_inbox {open_only?}`

**CLI** (`vk-cli/src/verbs.rs`):
- `vibeke msg @a [@b…] <text>`, also from stdin with `-`
- `vibeke msg reply <id> <text>`
- `vibeke msg done <id>`
- `vibeke msg inbox`
- `vibeke msg list`
- `vibeke name [@name] [--pane <target>] [--clear]`
- The API mapping (`vibeke <noun> <verb>`) has the nouns `mention` (`list`, `suggest`, `set`,
  `clear`) and `message` (`send`, `reply`, `complete`, `get`, `list`, `cancel`, `retry`), with
  the params of §3.1 as flags. `vibeke message send <body> --to <name>` repeats `--to` for
  several recipients.

The skill (`crates/vibeke/skill/SKILL.md`) gets a short Mentions section. The hook context
explains each delivery, so the skill stays small.

## 7. Web

The pane composer completes `@` against `mention.list`, like slash commands, and inserts the
name. Choosing an unnamed pane names it first. Named panes show `@name` where the pane label is
shown.

## 8. Tests

- `vk-proto` unit tests cover `valid`, `suggest` and `parse`.
- Integration tests (`crates/vibeke/tests/it/mentions.rs`) drive real hook calls through
  `vibeke hook claude …` with a fake agent run. They cover:
  - naming rules and conflicts;
  - a send, the envelope, the hook expanding it, a reply and the reply delivery;
  - a busy recipient that stays queued;
  - a spoofed envelope;
  - a leading mention that blocks the local prompt;
  - refusal across workspaces;
  - the hop limit;
  - the Stop reminder followed by completion.
- TUI unit tests cover the picker (entries, suggestion, insert) with both drawing paths left
  unchanged.

## 9. Later phases

- Phase 2:
  - groups (`@review-team`) with one combined result for the sender;
  - terminal targets that run a command in an idle shell and return output and exit status;
  - a Messages view in the TUI and the web app;
  - doctor checks for held deliveries.
- Phase 3:
  - tracked coordination (begin, assignments, one stall nudge, resolve with a report);
  - pinned git context with fresh, stale or unknown;
  - optional status summaries (spec 14);
  - pi, OpenCode and Gemini.
