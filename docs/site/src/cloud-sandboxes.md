# Cloud sandboxes

A **cloud sandbox** is a machine that a cloud provider runs for you. An agent works there instead of on your own computer. Your code and the agent's session move to the sandbox. You can bring them back at any time.

Cloud sandboxes help when you want an agent to run for a long time, run many agents at once, or work away from your own files.

Vibeke supports these providers: Sprites and E2B.

## Sign in

You sign in to a provider once for each host. Vibeke keeps the token in the system keychain of that host.

- In the terminal interface, open **sandboxes** from the command palette. Press `tab` until the provider is marked, then press `s`.
- From the command line, run `vibeke cloud login sprites`. Vibeke asks for the token without showing it.
- In the browser or desktop app, open **Sandboxes** and choose **Sign in** next to the provider. Choose **Get a token** to open the provider's page. Paste the token into the field.

Some providers also let you import a login that you already have. The sign-in screen shows each option that is available for the provider. If the host already has the token in an environment variable, the app tells you.

Any action that needs a sign-in opens the same screen. After you sign in, the action runs again.

## Start a task in a sandbox

To run a new task in a sandbox from the start, use the cloud isolation. Run this command in your repository, and give the task a title:

```sh
vibeke task new "Cloud task" --isolate cloud --provider sprites
```

Vibeke creates the sandbox, copies the task's checkout into it, and opens a pane there. The pane looks like any other pane. You can detach, reattach and review changes as usual.

## Send a pane to the cloud

You can also move work that is already running.

1. In the browser or desktop app, choose **Cloud** in the header of the workspace. In the terminal interface, open the menu of the pane and choose **send to cloud**. The command palette has the same command in both.
2. Choose a provider. Sign in if the app asks.
3. Choose **A new sandbox**. If the task of the pane already has a sandbox at this provider, you can choose that sandbox instead.
4. Confirm and follow the progress.

Vibeke waits until the agent finishes its current turn. Then it packs the work, uploads it, and starts the agent again in the sandbox. If the agent is busy, you can choose **Interrupt and hand off**. If any step fails, the pane keeps running on your host.

After a successful send, the agent works in the sandbox, and the pane on your host closes. Your uncommitted changes are now in the sandbox. Vibeke stashes them in the checkout on your host, with the message `vibeke: sent to <sandbox>`. A later bring-back then lands in a clean checkout. Run `git stash list` to see the stash.

If the provider rejects the sign-in during the move, for example because you revoked the token, the app asks you to sign in. Then it starts the same move one more time.

From the command line, use `vibeke cloud send`.

## Bring the work back

For a pane that runs in a sandbox, the header of the workspace shows **Bring back** instead of **Cloud**. You can also choose **Bring back from cloud…** in the pane menu or the command palette. You can bring the work back to:

- **This host.** The agent continues in a new pane on your host. The task uses your host again.
- **A paired host.** Vibeke sends the work to another of your hosts. That host needs to be paired with this one. See [Sharing and handoff](handoff.md).

From the command line, use `vibeke cloud bring-back`.

After the work is back, Vibeke keeps, suspends or destroys the sandbox. The setting `after_bring_back` in the `[cloud]` section of the configuration decides this.

## See and clean up sandboxes

Open **Sandboxes** in the app (the address `#/sandboxes`) or from the command palette of the terminal interface. On the command line, run `vibeke cloud ls`. Vibeke groups sandboxes by provider. Each row shows:

- the state, such as running or suspended;
- who owns it: yours, idle, orphaned, from another host, or missing;
- the task, the panes, the age and the last activity;
- a **Not synced** mark when the sandbox holds work that is not on your host.

Use the row actions to open, bring back, suspend, resume, checkpoint, adopt or destroy a sandbox. In the terminal interface, select a row with `j` and `k`, and use these keys:

| Key | Action |
| --- | --- |
| `enter` | Open the pane of the sandbox |
| `b` | Bring the work back |
| `p` | Suspend or resume |
| `c` | Save a checkpoint |
| `a` | Adopt |
| `f` | Forget |
| `d` | Destroy |
| `C` | Clean up |
| `tab` | Mark the next provider |
| `s` | Sign in to the marked provider |
| `r` | Read the list again |

An *orphaned* sandbox is one of yours whose task no longer exists. **Adopt** works on orphaned sandboxes and on sandboxes of another host. Vibeke makes a new task on your host. The task uses the branch of the sandbox, and Vibeke pulls that branch from the sandbox. Vibeke opens a pane for each session that still runs in the sandbox, and the app shows that pane. Sometimes the sandbox does not say which repository it holds. Then Vibeke asks for the path of that repository on your host. From the command line, run `vibeke cloud adopt <sandbox>`.

A *missing* sandbox is one that the provider no longer lists. **Forget** removes its record.

When you destroy a sandbox that has work that is not on your host, Vibeke stops and asks. You can choose **Bring back first**, or **Destroy anyway**.

**Clean up…** shows which orphaned and idle sandboxes Vibeke can destroy. Nothing is destroyed until you confirm. The confirmation destroys only the sandboxes in that list. Vibeke checks each of them again first. It skips a sandbox that no longer qualifies, for example one that now has work that is not on your host. From the command line, run `vibeke cloud prune --dry-run` first.

## Costs and limits

The provider bills you for sandboxes, not Vibeke. A running sandbox costs money. Check the prices of your provider.

- Vibeke suspends a sandbox that has had no session for 30 minutes. You can change this with `idle_suspend_after` in the `[cloud]` section.
- Vibeke destroys an idle sandbox automatically only when you set `idle_destroy_after`. It never does this when the sandbox has work that is not on your host.
- A provider may limit how long a sandbox can run, or how many you can have. The app shows the provider's message when a limit stops an action.

## Security

- A sandbox is a different machine on a different network. It sees only the clone of the task and the agent credentials that Vibeke copies for the agent in that pane.
- Your provider token stays on your host, in the keychain. It never goes into the sandbox.
- The token does not appear in logs, events, command arguments or environment variables.
- A pane cannot send itself to the cloud. An agent that asks to move work needs your approval first.
- Destroying a sandbox with work that is not on your host needs your explicit choice. Automatic clean-up never does this.

See the [security model](security.md) for the full picture.
