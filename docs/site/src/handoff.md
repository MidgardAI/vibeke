# Transfers and shared access

A **handoff** transfers an agent's work to another host. **Shared access** lets another person view or approve work on its current host.

The browser and desktop apps provide both features through the gateway.

## Requirements

[Pair the app](mobile.md) with a gateway first. Both hosts must be online for a transfer. Offline delivery is unavailable.

The destination needs a repository checkout, the agent CLI, and its own agent login.

## Transfer work to your host

1. Pair the app with both hosts using full access.
2. Open the source pane menu.
3. Select **Hand off this pane…**.
4. Select the destination host.
5. If the agent is active, wait for its turn to finish. To interrupt the turn, select **Interrupt and hand off** instead.
6. Review the branch, changes, files, conversation, and skipped items in the transfer summary.
7. Confirm the transfer.
8. If Vibeke cannot find the repository on your destination host, enter its path.

Vibeke creates a new worktree on the destination. It applies the changes and attempts to resume the agent.

A handoff copies work after an agent turn. It does not move process memory or close the source workspace. Both copies remain available.

## Transfer contents

| Item | Behavior |
| --- | --- |
| Git commits | A Git bundle includes local commits when necessary. |
| File changes | A patch includes uncommitted changes. |
| Untracked files | Includes eligible regular files up to 5 MiB each. Skips ignored files, symlinks, and recognized secret files. |
| Claude Code or Codex conversation | Uses an available transcript for native resume. Rewrites paths for the destination. |
| Other agents or missing transcripts | Transfers the files. A supported agent can start with a handoff note instead of the previous conversation. |
| Credentials | Does not transfer agent logins, subscriptions, or credential stores. |

The destination uses its own credentials. Secret filters and transcript redaction cannot identify every possible secret. Review the transfer summary before you send it.

Bundles have a 200 MiB limit. Temporary bundle storage expires after one hour.

## Transfer work to a teammate

The recipient prepares the destination:

1. Open **Settings → Receive a handoff**.
2. Create an invitation.
3. Send the invitation to the sender.
4. Make the repository available in a Vibeke workspace on the destination host.

The sender completes the transfer:

1. Open the invitation in the app.
2. Select the recipient's host in the source pane's handoff flow.
3. Review the summary.
4. Confirm the transfer.

The gateway matches the repository origin. The invitation does not permit access to other panes or arbitrary repository paths.

Vibeke imports the work into a new workspace. It does not start an agent automatically. The recipient receives a notification and decides when to resume.

## Share a pane or workspace

1. Select **Share this pane…** in the app.
2. Select the pane or its workspace.
3. Select **View** or **View + approve** access.
4. Set an expiry time.
5. Create the invitation.

Approval access permits answers to requests within the selected scope. It does not give full host administration access.

The gateway refuses expired shares and removes them from its device list. Use **Settings → Devices** to revoke access earlier. See [device recovery](mobile.md#trust-and-recovery).

## Pair your hosts with each other

A host can also pair with another host, so the two gateways can deliver work to each other directly. On the destination host, create a peer invitation; on the source host, redeem it:

```sh
vibeke gateway peer invite                    # on the destination: prints the command to run on the source
vibeke gateway peer add '<invitation link>'   # add --share-user to show your git name and email
vibeke gateway peer list
vibeke gateway peer remove <id or name>
```

A peer invitation pairs one of your own hosts and never expires. A teammate's handoff invitation can also be redeemed this way; that pairing expires with the invitation. A paired host can only deliver handoffs: it can't see panes, the inbox or other devices.

## Manage invitations

```sh
vibeke gateway invites        # unused invitation links, and the share, handoff and peer devices they created
vibeke gateway revoke <id>    # cancel an unused invitation, or revoke a device
```

Both list the kind, expiry, limit and owner of each entry. Every cancellation and revocation is recorded in the gateway's audit log. When you accept an invitation in the app, the app pairs with a separate key for it, so it never replaces your own pairing with that host.

## Recover from a connection failure

If the import result is unknown, inspect the destination before another attempt. The work can already be present.

If the import succeeds but the agent fails to start, the worktree remains available. Correct the agent setup before you resume.
