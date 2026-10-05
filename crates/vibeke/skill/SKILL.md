---
name: vibeke
description: Coordinate panes, agents, tasks and notifications in the Vibeke terminal runtime. Use only when the user mentions Vibeke or asks you to coordinate other panes/agents/tasks, and only when VIBEKE=1.
---

# Vibeke

Guard: run `test "${VIBEKE:-}" = 1` first. If it fails, you are not inside Vibeke — stop.

- Prefer `--current` / `@current` targets (your own pane). Never target `@focused`.
- Discover with `vibeke --help`, `vibeke <noun>` (verb list), `vibeke api methods`. Output is JSON when piped; parse it, never guess handles.

## Topology vs agents
A *pane* is a terminal; an *agent* is a recognized harness (claude, codex…) running in a pane. Agent state: `starting working idle error rate_limited exited`; `needs_approval`/`needs_answer` mean an open interaction; `done` = idle and not yet seen. A trailing `~` means the state is inferred from the screen.

## Delegate in isolation
When delegated work edits files, prefer an isolated task workspace:
`vibeke task new "fix login redirect" --agent claude:impl` (git worktree + branch + ports) over a sibling pane in the same directory.

## Coordinate agents
- `vibeke agent spawn reviewer --harness codex --split-of @current --prompt "…"`
- `vibeke agent prompt reviewer "…" --wait --timeout-ms 600000`
- `vibeke agent wait reviewer --until idle,needs_approval,needs_answer`
- `vibeke agent read reviewer --source recent --lines 80`

## Interactions
You can *see* other agents' open questions/approvals (`vibeke ask list`), but you cannot answer your own, and you should not answer other agents' approvals unless the user explicitly asked you to. Summarize them for the user instead.

## Ordinary commands
`vibeke pane split --current --direction right --no-focus`, then `vibeke pane run <pane> "pnpm test" --wait`, then `vibeke pane read <pane> --source recent_unwrapped`.

## Search and notify
- `vibeke search query "migration failed"` searches scrollback across panes.
- `vibeke notify "build finished" "all green"` raises a notification attributed to your pane.

## Safety
Don't close panes you didn't create; don't `vibeke server stop`; don't use `--force`; don't install integrations or change config without explicit instruction. Errors are JSON on stderr: exit 1 API error, 2 usage, 3 timeout.
