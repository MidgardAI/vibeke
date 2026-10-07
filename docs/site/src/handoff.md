# Transfers and shared access

A **handoff** transfers an agent's work to another host. **Shared access** lets another person view or approve work on its current host.

The browser and desktop apps provide both features through the gateway.

## Requirements

[Pair the app](mobile.md) with a gateway first. Both hosts must be online for a transfer. Offline delivery is unavailable. The app only starts a transfer: the source host sends the work to the destination itself, so you can close the app while it runs.

The destination needs the agent CLI and its own agent login. It does not need a checkout in advance: the recipient can choose an existing clone or clone the repository when accepting.

## Transfer work to your host

1. Pair the app with both hosts using full access.
2. Open the source pane menu.
3. Select **Hand off this pane…**.
4. Select the destination host. The list shows the hosts the source can already send to, then your
   other hosts. Choosing one of those pairs the two hosts first; the app does this for you, since
   it has full access to both.
5. If the agent is active, the app offers **Interrupt and hand off**, which ends the agent's
   current turn before packing the work.
6. Follow the progress, or close the sheet: the transfer continues in the background and the app
   tells you when it is delivered or failed. A failed transfer can be tried again from the sheet.

The destination keeps the work as an **incoming handoff**. Vibeke imports it right away when the
destination already has a clone of the repository and remembers where the last handoff for that
repository went (and **always ask** is off). Otherwise the destination shows a notification,
"Incoming handoff from <host>: <branch>", and the handoff waits until it is accepted.

Importing creates a new worktree on the destination. It applies the changes and attempts to resume
the agent. When the destination is one of your hosts in the app, **Open on <host>** opens the
imported pane there.

A handoff copies work after an agent turn. It does not move process memory or close the source workspace. Both copies remain available.

## Accept an incoming handoff

An incoming handoff waits on the destination for 7 days. In the app, **Handoffs** in the sidebar
lists the incoming handoffs of all your hosts, with a count of those that wait for you; the
notification opens the same list. Select one to see who sent it, the branch, the files, the
agent's last message, whether the conversation can be resumed, and the secret files that were not
sent (a checklist of what to bring yourself, such as `.env`).

Accepting it chooses:

- **The repository:** a clone that Vibeke found (any of its remotes must match the sender's
  origin), **Choose folder…** for another clone, or **Clone to…** for a new clone of the origin
  at a folder you choose, made with your own Git credentials. If the folder you choose is a clone
  of another repository, the app shows its remotes.
- **The worktree path and branch:** by default `<repo>-handoff-<branch>` next to the repository
  (or in the folder you used last time for that repository) on branch `handoff/<branch>`.
- **Whether to resume the agent**, and whether to trust the worktree's `mise` or `direnv`
  configuration when it has one.

Folders are picked from a list of the destination's folders: type to filter, Tab to complete.
In the desktop app on the destination itself, **Browse…** opens the system folder dialog.

If the import fails, nothing is left behind and the handoff stays available, so you can accept it
again with another choice. Declining a handoff deletes it. If the agent did not start, select
**Retry resume** on the imported handoff once the agent setup is fixed.

Handoffs from your own hosts import without asking when the repository and a place for it are
known. To review every handoff first, turn on **Settings → Sharing & handoff → Always ask before
importing handoffs** for that host.

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

1. Open **Settings → Sharing & handoff** and create a handoff invitation for the host that should
   receive the work.
2. Send the invitation to the sender.

The sender accepts the invitation on one of their hosts:

1. Open the invitation in the app.
2. Choose which of your hosts accepts it. Tick **Show my git name and email** to let the
   recipient see who is sending; otherwise they only see your host's name.
3. Hand off a pane on that host and select the teammate's host as the destination.

The invitation pairs that host with the teammate's host until the invitation expires. It lets the
host deliver handoffs and nothing else: it can't see panes, the inbox or other devices there, and
the sender cannot choose where the work lands on the recipient's host.

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

In the app, **Settings → Sharing & handoff** shows, for each of your hosts:

- pending invitations, which you can cancel;
- the hosts it sends handoffs to, with their owner and expiry, which you can remove;
- the devices and hosts invitations created (shares, handoff senders and paired hosts), with
  their kind, owner and expiry, which you can revoke.

From a terminal:

```sh
vibeke gateway invites        # unused invitation links, and the share, handoff and peer devices they created
vibeke gateway revoke <id>    # cancel an unused invitation, or revoke a device
```

Both list the kind, expiry, limit and owner of each entry. Every cancellation and revocation is recorded in the gateway's audit log. When you accept an invitation in the app, the app pairs with a separate key for it, so it never replaces your own pairing with that host.

## Recover from a connection failure

If the app loses the connection while starting a transfer, the transfer may still run on the
source host: open **Hand off this pane…** again or wait for the notice before another attempt.
On the destination, check **Handoffs**: the work can already be there; sending the same bundle
again does not create a second one.

If the import succeeds but the agent fails to start, the worktree remains available. Correct the
agent setup, then resume the agent from the imported handoff.
