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

In the terminal client, press `prefix+alt+h` (**Hand off this pane to another host…**,
`handoff_send`), or right-click the pane's row in the sidebar and choose **Hand off…**. Pick one
of the hosts this one is paired with, tick **Interrupt agent if busy** if needed and press
`enter`. When the terminal client is attached to several of your machines, the ones this host is
not paired with yet are listed under **Your machines (will pair)**: choosing one pairs the two
hosts first, then hands off. The tab bar shows the progress ("⇢ marvin 42%"), and a notice says
when the work was delivered or imported. Cancel a send from **Incoming handoffs** (`x`). A
teammate's invitation is accepted in **Sharing & handoff** (see below).

### From a shell pane

Inside a pane, `vibeke handoff send <peer>` asks for your approval first, so an agent in a pane
can never move code to another host without you knowing:

```sh
vibeke handoff send marvin            # the pane you run it in
vibeke handoff send marvin --pane <pane>  # another pane of the same workspace
vibeke handoff send marvin --no-wait  # print the request id and return
```

The command prints `Waiting for approval in Vibeke (prefix+shift+e, or the notice in the tab bar)…`
and waits. Vibeke shows what will be sent, worked out by Vibeke itself: the pane, its repository,
branch, number of changed files, its agent (or none) and the destination host. Text the command
passed with `--reason` is shown separately, as unverified. Approve once, approve **always** (the
same pane may send to the same host again without asking until its process restarts), or deny.
You can also decide outside any pane with `vibeke auth approval <request> approve|always|deny`;
`vibeke auth list` shows open requests and standing approvals. Once approved, the command prints
the queued job as if you had sent it yourself; a denial exits with an error that says "denied",
and Ctrl-C withdraws the request.

When the pane you run it in has no agent and its workspace has exactly one agent pane, the
command asks whether to send that agent pane instead. A shell pane without an agent sends its
repository with no conversation. If the pane switches to another branch, commit or repository
before the transfer starts, the transfer fails with `repo_moved` and nothing is sent; ask again.
`vibeke handoff cancel <job>` and `vibeke handoff redeem <link>` ask the same way inside a pane.
Revoking the pane's access (`vibeke pane revoke-token`) withdraws its requests and ends its
standing approvals.

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

### In the terminal client

A waiting handoff shows in the attention inbox (`prefix+i`) as "Incoming handoff from <host>:
<branch>", and the tab bar shows `⇣N` while N handoffs wait. Select it and press `enter`, or open
**Incoming handoffs** (`prefix+shift+h`, a click on `⇣N`, or `handoffs` in the command palette),
to see what arrived: the sender, the repository and branch with its head commit,
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
For a pane an import created, right-click its sidebar row and choose **Handoff details**
(`handoff_details`) to see where it came from and retry the agent.

To review every handoff first in the terminal client, open **Sharing & handoff** (`sharing` in
the command palette) and press `a` for **Always ask before importing handoffs**.

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

1. Open **Settings → Sharing & handoff** and create a handoff invitation for the host that should
   receive the work.
2. Send the invitation to the sender.

The sender accepts the invitation on one of their hosts:

1. Open the invitation in the app.
2. Choose which of your hosts accepts it. Tick **Show my git name and email** to let the
   recipient see who is sending; otherwise they only see your host's name.
3. Hand off a pane on that host and select the teammate's host as the destination.

In the terminal client, the recipient opens **Sharing & handoff** (`sharing` in the command
palette) on the receiving host and presses `n`, then `t` (**Invite a teammate to send to me**):
the link shows with a QR code, and `c` copies it. The sender opens **Sharing & handoff** on their
host, presses `p` and pastes the link. The view says what it is before anything happens
("Handoff invitation from laptop-anna (teammate), valid 23h"); tick **Show my git name and
email** if you like, then **Accept**. A link that isn't an invitation for a host, or that
expired, is refused without contacting anyone.

The invitation pairs that host with the teammate's host until the invitation expires. The app
itself is never paired for it: opening the link in an app only offers to accept it on one of your
hosts, and the recipient's host refuses a claim from anything but a host. It lets the host deliver
handoffs and nothing else: it can't see panes, the inbox or other devices there, and
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

`vibeke handoff redeem '<invitation link>'` does the same as `vibeke gateway peer add`; inside a
pane it asks for your approval first.

In the terminal client, **Sharing & handoff** does the same: `n` then `h` (**Pair another of my
hosts**) on the destination shows the invitation, and `p` on the source pastes and accepts it.
When the terminal client is attached to both hosts, choosing the other host under **Your machines (will pair)** when handing off pairs them for you.

A peer invitation pairs one of your own hosts and never expires. A teammate's handoff invitation can also be redeemed this way; that pairing expires with the invitation. A paired host can only deliver handoffs: it can't see panes, the inbox or other devices.

## Manage invitations

In the app, **Settings → Sharing & handoff** shows, for each of your hosts:

- pending invitations, which you can cancel;
- the hosts it sends handoffs to, with their owner and expiry, which you can remove;
- the devices and hosts invitations created (shares and paired hosts, including a teammate's), with
  their kind, owner and expiry, which you can revoke.

In the terminal client, **Sharing & handoff** (`sharing` in the command palette) shows the same
sections for the host it is attached to (`m` switches machine): **Peers**, **Invitations** and
**Invited devices**, where `x` removes, cancels or revokes the selected entry after a
confirmation. It reaches the gateway through the host's server; when the gateway isn't running
it says so ("start it with `vibeke gateway on`").

From a terminal:

```sh
vibeke gateway invites        # unused invitation links, and the share and peer devices they created
vibeke gateway revoke <id>    # cancel an unused invitation, or revoke a device
```

Both list the kind, expiry, limit and owner of each entry. When you accept a share in the app, the app pairs with a separate key for it, so it never replaces your own pairing with that host.

## Recover from a connection failure

If the app loses the connection while starting a transfer, the transfer may still run on the
source host: open **Hand off this pane…** again or wait for the notice before another attempt.
On the destination, check **Handoffs**: the work can already be there; sending the same bundle
again does not create a second one.

If the import succeeds but the agent fails to start, the worktree remains available. Correct the
agent setup, then resume the agent from the imported handoff: `r` in the terminal client's accept
overlay or handoffs list, or `vibeke handoff resume <id>`.
