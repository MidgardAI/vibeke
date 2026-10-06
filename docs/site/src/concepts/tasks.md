# Tasks and review

A **task** is a unit of agent work with its own worktree (a task workspace), ports and setup. Tracking can be explicit (`vibeke task create`) or attached to a normally launched CLI. A task records its intent and success criteria, gathers evidence (diffs, check runs, transcript highlights) and ends in **review**: `task review get|diff|accept`, with checks that need your authorization before they run (`task check authorize|run`). Reviewer agents and dependency links between tasks are available from the T4 stage. Agents may report observations but cannot confirm intent, accept a review or authorize a check.

The attention list ranks what needs a decision across all tasks.
