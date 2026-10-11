# Work with a teammate

Send an agent's work to a teammate's host, or let a teammate watch or approve one pane or workspace. Both features use the gateway, so [pair the app](mobile.md) with a host first. To move work between your own hosts, see [move work to another host](handoff.md).

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

In the terminal client, the recipient opens **Connections → Hosts** (`prefix+alt+d`, then
`tab`) on the receiving host and presses `n`, then `t` (**Invite a teammate to send to me**):
the link shows with a QR code, and `c` copies it. The sender opens **Connections → Hosts** on their
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
receives a notification, then accepts it (choosing the repository, worktree and branch, as described in [move work to another host](handoff.md#accept-an-incoming-handoff))
and decides whether to resume the agent, or declines it.

## Share a pane or workspace

In the app:

1. Select **Share this pane…** in the app.
2. Select the pane or its workspace.
3. Select **View** or **View + approve** access.
4. Set an expiry time.
5. Create the invitation.

In the terminal client, open **Connections → People** (`prefix+alt+d`, then `tab`), or run
`share_pane` (**Share this pane or workspace with someone…**, `prefix+alt+v`, also in the
sidebar pane's right-click menu) to open the form for the focused pane:

1. Choose **This pane** or **This workspace**.
2. Choose **View**, **View + approve** or **Control**. Control asks for a confirmation.
3. Choose the expiry: 1h, 2h, 8h, 24h or 7d.
4. Optionally enter a name for the person.
5. Create the share. The view shows a link and a QR code to send them; they open it in the browser app.

**People** also lists the shared access on this host (name, pane or workspace, access, expiry)
and unused share links; `x` revokes the selected entry after a confirmation. It reaches the
gateway through the host's server, and says so when the gateway isn't running.

What each level allows, always limited to the shared pane or workspace:

- **View**: the screen, the agent's conversation, git status, diffs and log, and the files of the
  pane's repository (secret-looking files and `.git` stay hidden).
- **View + approve**: also answering the agent's questions and permission requests, and
  interrupting it. An approved request runs as you; when the agent isn't sandboxed, that can
  reach anything your account can.
- **Control** (terminal client): also typing into the pane and prompting its agent. This is like
  a shell as you on that machine: unless the pane runs in a sandbox, they can read and change any
  file you can. Share it only with someone you trust, and keep the expiry short.

No level can create or revoke access, hand work off, or pair hosts.

Whoever opens a share link first gets the access: send it privately.

## Revoke access

The gateway refuses expired shares and removes them from its device list. Use **Settings → Devices** in the app, or the **People** tab of **Connections** in the terminal client, to revoke access earlier. See [manage devices](mobile.md#manage-devices).

You can also list and revoke invitations from the command line. See [manage invitations](handoff.md#manage-invitations).

## Next steps

- [Move work to another host](handoff.md) to transfer work between your own machines.
- [Phone and browser access](mobile.md) for device pairing, access levels and recovery.
- [Security model](security.md) to see what a shared pane can reach.
