# Transfers and shared access

A **handoff** transfers an agent's work to another host. **Shared access** lets another person view or approve work on its current host.

The browser and desktop apps provide both features through the gateway.

## Requirements

[Pair the app](mobile.md) with a gateway first. Both hosts must be online for a transfer. Offline delivery is unavailable.

The destination needs the agent CLI and its own agent login. It does not need a checkout in advance: the recipient can choose an existing clone or clone the repository when accepting.

## Transfer work to your host

1. Pair the app with both hosts using full access.
2. Open the source pane menu.
3. Select **Hand off this pane…**.
4. Select the destination host.
5. If the agent is active, wait for its turn to finish. To interrupt the turn, select **Interrupt and hand off** instead.
6. Review the branch, changes, files, conversation, and skipped items in the transfer summary.
7. Confirm the transfer.

In the terminal client, run **Hand off this pane to another host…** (`handoff_send`) from the
command palette: pick one of the hosts this one is paired with, tick **Interrupt agent if busy**
if needed and press `enter`. The tab bar shows the progress ("⇢ marvin 42%"), and a notice says
when the work was delivered or imported. Cancel a send from **Incoming handoffs** (`x`). Pairing
with a teammate's invitation happens on the command line (`vibeke gateway peer add <link>`).

The destination keeps the work as an **incoming handoff**. Vibeke imports it right away when the
destination already has a clone of the repository and remembers where the last handoff for that
repository went (and **always ask** is off). Otherwise the destination shows a notification,
"Incoming handoff from <host>: <branch>", and the handoff waits until it is accepted.

Importing creates a new worktree on the destination. It applies the changes and attempts to resume
the agent.

A handoff copies work after an agent turn. It does not move process memory or close the source workspace. Both copies remain available.

## Accept an incoming handoff

An incoming handoff waits on the destination for 7 days. Accepting it chooses:

- **The repository:** a clone that Vibeke found (any of its remotes must match the sender's
  origin), another clone by path, or a new clone of the origin at a folder you choose, made with
  your own Git credentials.
- **The worktree path and branch:** by default `<repo>-handoff-<branch>` next to the repository
  (or in the folder you used last time for that repository) on branch `handoff/<branch>`.
- **Whether to resume the agent**, and whether to trust the worktree's `mise` or `direnv`
  configuration when it has one.

If the import fails, nothing is left behind and the handoff stays available, so you can accept it
again with another choice. Declining a handoff deletes it. If the agent did not start, resume it
from the imported handoff once the agent setup is fixed.

### In the terminal client

A waiting handoff shows in the attention inbox (`prefix+i`) as "Incoming handoff from <host>:
<branch>". Select it and press `enter`, or open **Incoming handoffs** from the command palette
(`handoffs`), to see what arrived: the sender, the repository and branch with its head commit,
the untracked files, the secret files the sender kept back ("bring your own: .env"), the agent's
last message and whether the conversation resumes. Then choose:

- **Repository:** a clone Vibeke found, **Browse…** for another clone, or **Clone to…** a new
  folder (by default next to the clone Vibeke suggests, or in `~/code`).
- **Worktree** and **Branch:** prefilled; `enter` on the worktree opens the folder picker (`tab`
  completes, arrows walk the folders, git repositories are marked), the branch is typed in place.
- **Resume agent**, **Trust mise config** and **Trust direnv config**.

`a` (or **Accept**) imports it. The overlay shows the progress (cloning, importing, starting the
agent) and then focuses the new pane. If the import fails, the reason stays on screen so you can
choose again; a clone whose remotes don't match the sender's origin is listed with its remotes.
If the agent did not start, press `r` to retry once the agent setup is fixed. `d` declines the
handoff; `esc` (**Later**) leaves it waiting.

The **Incoming handoffs** list also shows imported and failed handoffs and the panes you are
sending; there `d` declines, `r` retries the agent of an imported handoff and `x` cancels a send.

### From the command line

```sh
vibeke handoff incoming                         # id, state, sender, repository, branch, age
vibeke handoff accept <id>                      # into the suggested clone and worktree path
vibeke handoff accept <id> --repo ~/code/app --worktree ~/code/app-fix --branch fix/login
vibeke handoff accept <id> --clone-to ~/code/app --no-resume --trust mise,direnv
vibeke handoff decline <id>
vibeke handoff resume <id>                      # start the agent of an imported handoff again
vibeke handoff prefs --always-ask on            # handoffs from your own hosts always wait
```

Without `--repo` or `--clone-to`, `accept` uses the clone (and worktree path) Vibeke suggests and
stops with an error when there is none. The apps use the same `handoff.incoming.list` and
`handoff.accept` API methods, through the gateway from a fully paired device.

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

Bundles have a 200 MiB limit. A transfer that never completes is discarded after one hour; a
delivered incoming handoff is kept for 7 days unless it is imported or declined first.

## Transfer work to a teammate

The recipient prepares the destination:

1. Open **Settings → Receive a handoff**.
2. Create an invitation.
3. Send the invitation to the sender.

The sender completes the transfer:

1. Open the invitation in the app.
2. Select the recipient's host in the source pane's handoff flow.
3. Review the summary.
4. Confirm the transfer.

The invitation does not permit access to other panes, and the sender cannot choose where the work
lands on the recipient's host.

The handoff arrives as an incoming handoff and is never imported automatically. The recipient
receives a notification, then accepts it (choosing the repository, worktree and branch, as above)
and decides whether to resume the agent, or declines it.

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

If the delivery result is unknown, inspect the destination's incoming handoffs before another
attempt. The work can already be there; sending the same bundle again does not create a second
one.

If the import succeeds but the agent fails to start, the worktree remains available. Correct the
agent setup, then resume the agent from the imported handoff: `r` in the terminal client's accept
overlay or handoffs list, or `vibeke handoff resume <id>`.
