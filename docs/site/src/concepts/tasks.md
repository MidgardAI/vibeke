# Tasks and review

A **task** records agent work. It can have a separate worktree, ports, and setup commands.

Use `vibeke task new` to create an isolated task. You can also track an agent that you already started. Tracking records the task without moving its files or process.

## Task requirements

A task stores its intent, success criteria, and agent run links. You can edit the task details without sending a new instruction to the agent.

To change the agent's instructions, send an explicit clarification. The saved task details and the delivered message remain separate records.

## Review the result

The task collects diffs, check results, and transcript evidence.

1. Use `task review get` to inspect the review.
2. Use `task review diff` to read the changes.
3. Use `task check authorize` to approve a check.
4. Use `task check run` to run the approved check.
5. Use `task review accept` to accept the result.

Agents can report observations. They cannot confirm task intent, authorize checks, or accept reviews.

## Review agents

`task reviewer` prepares a request and shows its prompt. It does not start an agent.

After you confirm the request, use `task reviewer-start` to start the review agent. Review findings remain separate from check evidence.

## Task dependencies

Use `task depend` to add a blocking or related task link. Vibeke rejects cyclic dependencies. The attention list orders tasks that need a decision.

## Agents sharing one checkout

When several agents work in one git checkout, Vibeke shows it instead of forbidding it. The collision tracker is advisory: it warns, and it never blocks, reverts, or reassigns a change.

Vibeke tracks git checkouts only. It does not track agents that work in a directory outside a repository, or in your home directory. Files there are mostly program state, such as agent settings and databases, and their changes are not agents' collisions.

A collision is raised when:

- two runs report edits of the same file (`high`);
- a run reports an edit inside a glob another run claimed (`high`);
- a run reports an edit of a file another run read in the last ten minutes (`medium`);
- two runs report edits of different files in one directory (`low`).

Reports from the agents are the main signal. Some edits are not reported, for example edits by shell commands and formatters. For these edits, Vibeke watches the checkout and polls `git status` while an agent works there. A change is the edit of the run that reported a tool call on that path. Otherwise Vibeke guesses: the change is probably the edit of the only run that was working. When two or three runs were working, the change could be from any of them. When more runs were working, Vibeke does not guess. A change is also not counted when the run on the other side of the collision could have made it, for example when an agent formats a file it just edited.

A guess never raises a `high` collision. It raises at most a `medium` collision, which says that agents "may" have edited a file. Vibeke does not send a notification for a guess. A change nobody was working on is not an agent's collision.

Each path of a collision goes away when no run touches it for 30 minutes (`[collision] window`). The collision closes when it has no paths left, when fewer than two of its runs are still running, or when its checkout is no longer a git checkout.

A pane in a `high` or `medium` collision shows `⚠`. Under the workspace, the sidebar shows one line for the most severe collision, for example "claude and codex both edited `src/auth.ts`". It counts the other collisions as "+N more". The collision view lists every collision, including `low` ones. Open it with the `collisions` palette action. It lists the paths, the runs, and a timeline, and offers:

- **Pause** one run, with the adapter's own interrupt.
- **Tell the agents** with a short message. Vibeke uses only a native steer or follow-up channel, or Claude's next hook. It never types into a terminal mid-turn. A run with no such channel is told so.
- **Start a fresh task from here.** Vibeke creates a task from the shared checkout's `HEAD` and starts a new run there with a hand-off prompt. The original runs keep working and nothing is moved.
- **Ignore** a path.

`vibeke claim add "src/auth/**" --run a12` says which part of the checkout a run works in (a run without a task gets a run claim; `vibeke collision claim` is the same call, and a task's own claims count too). Another run writing there raises a `high` collision at once. With `[collision] enforce_claims = true`, Claude also denies its reported edit tools inside another run's claim. This is a courtesy guardrail: shell commands and other harnesses are unaffected. Claims end with their run.

Use `vibeke collision list` and `vibeke task get <task> --collisions` to read collisions from the command line. See [the `[collision]` settings](../reference/config.md).

See the [task commands](../reference/cli.md#vibeke-task) for arguments. See [transfers and shared access](../handoff.md) to transfer work to another host.
