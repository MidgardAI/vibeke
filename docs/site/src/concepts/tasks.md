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

Dependency links do not merge branches or deploy results automatically.

## Agents sharing one checkout

When several agents work in one directory, Vibeke shows it instead of forbidding it. The collision tracker is advisory: it warns, and it never blocks, reverts, or reassigns a change.

A collision is raised when:

- two runs write the same file (`high`);
- a run writes inside a glob another run claimed (`high`);
- a run edits a file another run read in the last ten minutes (`medium`);
- two runs write different files of one directory (`low`, a sidebar hint only).

Reports from the agents are the main signal. For edits no agent reported, such as shell commands and formatters, Vibeke watches the checkout and polls `git status` while an agent works there. It attributes a change to the run that reported a tool call on that path, otherwise to the runs working there, and it marks the change `ambiguous` when more than one run could have made it. A change nobody was working on is not an agent's collision.

A pane in a collision shows `⚠`, and the sidebar names the paths ("2 agents editing `src/auth.ts`"). Open the collision view with the `collisions` palette action. It lists the paths, the runs, and a timeline, and offers:

- **Pause** one run, with the adapter's own interrupt.
- **Tell the agents** with a short message. Vibeke uses only a native steer or follow-up channel, or Claude's next hook. It never types into a terminal mid-turn. A run with no such channel is told so.
- **Start a fresh task from here.** Vibeke creates a task from the shared checkout's `HEAD` and starts a new run there with a hand-off prompt. The original runs keep working and nothing is moved.
- **Ignore** a path.

`vibeke claim add "src/auth/**" --run a12` says which part of the checkout a run works in (a run without a task gets a run claim; `vibeke collision claim` is the same call, and a task's own claims count too). Another run writing there raises a `high` collision at once. With `[collision] enforce_claims = true`, Claude also denies its reported edit tools inside another run's claim. This is a courtesy guardrail: shell commands and other harnesses are unaffected. Claims end with their run.

Use `vibeke collision list` and `vibeke task get <task> --collisions` to read collisions from the command line. See [the `[collision]` settings](../reference/config.md).

See the [task commands](../reference/cli.md#vibeke-task) for arguments. See [transfers and shared access](../handoff.md) to transfer work to another host.
