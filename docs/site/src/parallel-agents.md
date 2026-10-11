# Run agents in parallel

This guide shows how to run several agents on one repository, keep their work apart, and find the agent that needs you.

## Give each agent its own task

A **task** is the safest way to run parallel agents. Each task gets its own git worktree, branch, and port block. Agents that stay in their own worktree do not change each other's files. A worktree is not a security boundary. An agent at the host execution level can still write anywhere your user can. Use a [sandbox execution level](concepts/sandboxes.md) when you need a real boundary.

1. Press **Ctrl+B**, then **Shift+K** to open **New task (git worktree)**. You can also run this command in a pane:

   ```sh
   vibeke task new "Fix the login form" --repo . --agent claude
   ```

2. Repeat for each piece of work. Use a different title and, if you like, a different agent such as `codex`.
3. Press **Ctrl+B**, then **g** to find any workspace, tab, agent, or task.

Vibeke creates the worktree under `~/.vibeke/worktrees` by default. Set `root = "sibling"` under `[tasks]` to place it next to the repository instead. See [tasks and review](concepts/tasks.md).

## Let agents share one checkout

You can also start `claude` and `codex` in panes of the same directory. Vibeke then warns you when two agents touch the same files. The warning is advisory. It never blocks or reverts a change.

- A pane in a serious collision shows `⚠`.
- The sidebar shows one line under the workspace, for example "claude and codex both edited `src/auth.ts`".
- The collision view lists every collision, with pause, tell, fresh task, and ignore actions. It has no default key. Click the sidebar line, or choose **collisions** in the command palette (**Ctrl+B**, then **:**).

To avoid more collisions, give one of the agents its own task. In the collision view, **Start a fresh task from here** creates a new worktree from the current commit. It starts a new agent there with a summary of the work. The original agents keep running, and their changes stay in the shared checkout. Pause or stop one of them yourself.

You can also tell Vibeke which part of the checkout a run owns:

```sh
vibeke claim add "src/auth/**" --run a12
```

See [agents sharing one checkout](concepts/tasks.md#agents-sharing-one-checkout) for how Vibeke decides.

## Type into several panes at once

Synchronized input sends every key and paste to several panes in the same tab. Use it for the same command on many shells.

1. Focus a pane in the tab.
2. Press **Ctrl+B**, then **Shift+S**. A `SYNC n` badge appears on the right of the tab bar. The number counts the panes that receive input.
3. Type. The other terminal panes of the tab receive the same input.
4. Press **Ctrl+B**, then **Shift+S** again to stop. You can also click the badge.

To add or remove one pane, focus it and press **Ctrl+B**, then **Alt+S**.

Agent panes are left out, so that a shell command never reaches many agents by accident. Press **Ctrl+B**, then **Alt+S** in an agent pane to add it. To include agents by default, set this:

```toml
[ui.sync_input]
include_agents = true
```

Browser panes and popups never take part. From scripts, use `vibeke pane sync-input start|stop|status`.

## Ask another agent to review

1. Run `vibeke task reviewer <task>`. This prints the review prompt. It does not start an agent.
2. Read the prompt. Then confirm it with `vibeke task reviewer-start <request> <prompt_digest>`.
3. Read the findings with `vibeke task notes <task>`.

Findings are an agent's opinion. They stay separate from check results. See [review agents](concepts/tasks.md#review-agents).

## Order tasks with dependencies

If one task needs another to finish first, link them:

```sh
vibeke task depend <task> <depends-on> --kind blocks
```

Vibeke rejects a link that would form a cycle. `vibeke task deps <task>` lists the links.

## Find the agent that needs you

- **Ctrl+B**, then **i** opens the attention inbox. It lists permission requests, questions, and plan reviews.
- **Ctrl+B**, then **a** finds the next agent that needs attention.
- **Ctrl+B**, then **Shift+A** also moves focus to that agent.
- **Ctrl+B**, then **Alt+A** lists every agent on every machine, ordered by attention. Press Enter to jump to one.

See [agents and interactions](concepts/agents.md).

## Know when an idle agent still works

An agent can leave a development server or a watcher running after its turn. Vibeke does not show that agent as done. The sidebar shows `⧗` and a label such as `idle · 2 bg`. See [background work](concepts/agents.md).

## Clean up

When a task is done, finish it:

```sh
vibeke task finish <task> --remove-worktree
```

- `vibeke task archive <task>` stops the agents, deletes the worktree, and keeps the branch.
- `vibeke task forget <task>` drops the record of a task whose checkout is missing. It deletes no files.
- Vibeke refuses to delete a worktree that has uncommitted changes or unpushed commits. This matches the default `protect_dirty = true` under `[tasks.cleanup]`. Add `--force` to delete it anyway.

See the [task commands](reference/cli.md#vibeke-task) and the [`[tasks]` settings](reference/config.md).

## Next steps

- [Tasks and review](concepts/tasks.md): inspect diffs, run checks, and accept a result.
- [Agents and interactions](concepts/agents.md): answer requests from the inbox.
- [Customize keys and appearance](customize.md): change the keys used in this guide.
