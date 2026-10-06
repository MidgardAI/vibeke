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

See the [task commands](../reference/cli.md#vibeke-task) for arguments. See [transfers and shared access](../handoff.md) to transfer work to another host.
