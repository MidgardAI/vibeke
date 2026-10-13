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

A link made in the app or the People form works for the first person who opens it. Send it privately. To limit a link to named people, use a GitHub link (see below).

## Limit a link to GitHub accounts

A **GitHub link** opens only for the GitHub accounts you name. The person who opens it signs in with GitHub first. Vibeke checks the account id, not the name, so a renamed account keeps its access.

- You can name accounts, name a [team](#teams), or allow any GitHub account.
- One link works for several people. It opens up to 20 times by default (at most 100) until it expires. Each person becomes a separate device with their own GitHub login.
- Your host must be signed in to an account (`vibeke login`). Without it, Vibeke refuses to make the link. It never falls back to a link that anyone can open.
- Opening the link in an old app shows "This link needs a newer Vibeke app".
- The desktop app opens GitHub links in your system browser.

### Your sharing list

By default, GitHub links are for the people and teams on your **sharing list**. Manage the list on your account page at [cloud.vibeke.dev/account](https://cloud.vibeke.dev/account):

1. Sign in with GitHub.
2. Under **People who can open your links**, type a GitHub username and select **Add**.
3. Select **Remove** next to a person to take them off the list.
4. Under **Teams**, tick each team that your links are for. A new team is ticked when you join it.

The list belongs to your account, so every host that is signed in to it uses the same list. A change applies to links you make after it. A link you already made keeps the people and teams it was made for.

To use a different list on one machine, add the GitHub logins to that machine's config. The config list then replaces your account's list on that machine:

```toml
[sharing]
github = ["alice", "bob"]
```

### Teams

A **team** is a group of people on the account server. Everyone in a team can open the GitHub links that a teammate makes for the team. You do not need to add each teammate to your sharing list.

1. On your [account page](https://cloud.vibeke.dev/account), create a team and give it a name. You become its owner.
2. Invite people. Owners can do this in two ways:
   - Type a GitHub username. That person sees the invitation on their own account page and selects **Join**.
   - Make an invite link and post it, for example in your team's chat. Anyone who has the link can join, after they sign in with GitHub. The link works for 7 days. You see it only once, and you can revoke it.
3. On the team page, owners can make another member an owner, remove members, rename the team or delete it. Anyone can leave a team.

Under **People who can open your links**, each of your teams has a tick box (**Everyone in** the team). A ticked team is included when you make a GitHub link without naming people or teams. New teams start ticked.

Vibeke checks who is in the team when someone opens a link, not when you make it. A person who joins the team later can open links you made before. A person who leaves the team, or whom an owner removes, cannot open them any more.

To make a link for a team, pick the team in the **Who** field of the **People** form, or pass `--team`. You can name a team by its name or by its id. You can only name teams that you are in.

## Share a running app for design review

A **design review link** lets a designer try one preview in their own browser. They do not need a terminal or an account on your host.

1. The agent or you declare the preview (see [previews and browsers](concepts/previews.md#share-a-preview-for-design-review)).
2. Create the link: `vibeke share review v4`, or use the **People** form and choose **Design review**.
3. Send the link. The designer opens it and signs in with GitHub when the link asks for it.

The designer can scroll, type and use the page as usual. They pick elements or areas, write a note for each pick and send the notes. When the agent changes the app, the designer sees the new version at the same link. Requests to your app travel end-to-end encrypted through your host.

A review link reaches only that one preview. It cannot reach terminals, files, git, agents or other previews.

### Notes: direct or held

- **Direct** (default for GitHub links): the notes go to the agent when the designer sends them.
- **Held** (default for links that anyone can open): the notes wait for you. You get a notification, such as "3 review notes from @alice on v4". You see the notes in the preview's notes list, with **Send to agent** and **Discard**. The designer sees "waiting for" you until you decide. Only your own devices can release held notes.

Each designer has their own notes. Two designers on one preview never change each other's notes. The agent sees who wrote them.

## Share from the command line

These commands work from any terminal on the host, and from programs that run on the host outside a pane, such as a chat bot. They need the gateway (`vibeke gateway on`). An agent inside a pane cannot create links.

```sh
vibeke share watch --workspace <workspace>      # a link to watch a workspace
vibeke share watch --pane <pane> --scope approve # watch and approve one pane
vibeke share review v4                           # a review link for preview v4
vibeke share review v4 --notes held --ttl 8h
vibeke share list                                # open links and the devices they made
vibeke share revoke <id>                         # close a link and every device it made
```

Choose who may open the link:

| Option | Who can open it |
| --- | --- |
| none | The people and teams on your sharing list (or the people in `[sharing] github` when the config has a list) |
| `--github alice,bob` | Those GitHub accounts |
| `--team Acme` | Everyone in that team. Repeat the option or use commas for more teams. |
| `--github alice --team Acme` | Those GitHub accounts and everyone in that team |
| `--any-github` | Any GitHub account |
| `--link` | The first person who opens it, like a link made in the app |

Other options: `--uses N` (how many people may open it), `--ttl 30m|24h|7d` (how long it works), `--label` (a name shown in **People**), `--scope view|approve` (for `watch`) and `--notes direct|held` (for `review`). `--pane @current` and `--workspace @current` mean the pane you run the command in.

The commands print only the link. Add `--json` to see the full result. If your sharing list is empty and you pass no option, the command stops. It asks you to add people or tick a team on your account page, or to pass `--github` or `--team`. If you name a team that you are not in, the command stops too. Use `--any-github` and `--link` with care: they let more people in.

## See who has access

**People** (`prefix+alt+d`, then `tab`) lists each device with its GitHub login, and groups the devices under the link they came from. `x` revokes the selected entry. Revoking a link revokes every device it made. The **People** form (`prefix+alt+v`) has a **Kind** field (**Watch** or **Design review**) and a **Who** field (**Anyone with the link**, **GitHub** with your default people and teams, **Team:** and the name of each of your teams, or **Any GitHub account**). For a review it also has **Notes**. A created link shows how many uses are left.

## Revoke access

The gateway refuses expired shares and removes them from its device list. Use **Settings → Devices** in the app, or the **People** tab of **Connections** in the terminal client, to revoke access earlier. See [manage devices](mobile.md#manage-devices).

You can also list and revoke invitations from the command line. See [manage invitations](handoff.md#manage-invitations).

## Next steps

- [Move work to another host](handoff.md) to transfer work between your own machines, or to send a session as a link.
- [Previews and browsers](concepts/previews.md) to declare the app you share for review.
- [Phone and browser access](mobile.md) for device pairing, access levels and recovery.
- [Security model](security.md) to see what a shared pane can reach.
