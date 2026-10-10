# Phone and browser access

Open [app.vibeke.dev](https://app.vibeke.dev) on your phone or computer.
The browser app connects to your Vibeke host through a relay.
The relay carries encrypted traffic. Your host makes an outbound connection and needs no inbound port.
Two hosted relays exist: `cloud.vibeke.dev` requires a Vibeke account, and `relay.vibeke.dev` still accepts any host without one.

For local desktop access, use the [desktop connection guide](desktop.md).

## Account

`cloud.vibeke.dev` requires a Vibeke account. On your host, run `vibeke login` once.
It prints a URL and a short code. Open the URL on any device, sign in with GitHub and confirm the code; the terminal finishes by itself.
No browser is needed on the host, so this works over SSH. Use `vibeke whoami` to see the signed-in account and `vibeke logout` to sign out.

## Pair your device

1. [Install the CLI](install.md) on your host and start a session with `vibeke`.
2. In a shell on that host, run:

   ```sh
   vibeke gateway pair \
     --relay https://cloud.vibeke.dev \
     --app-url https://app.vibeke.dev
   ```

   If you have not signed in, this command starts the login flow first.
   With `--relay https://relay.vibeke.dev` no account is needed.
   It saves the connection settings, enables gateway autostart, and asks the server to start it.
   Keep the command running while you pair.

3. Scan the QR code on your phone, or open the printed link in your browser.
4. Compare the device fingerprint with the fingerprint shown by the pairing command.
5. Confirm pairing on the device and in the terminal when prompted.

After the first setup, pair another device from the TUI or the shell.

In the TUI, press `prefix` then `alt+d` to open **Connections** on its **Devices** tab, or run **Pair a phone** from the command palette.
Choose the access level and press `enter`.
When the relay needs a Vibeke account and you are not signed in, the view first shows a sign-in code with a QR code.
Open the link on any device, sign in with GitHub and confirm the code; the pairing link follows by itself.
The list also shows the signed-in account, and `s` starts the sign-in on its own.
Scan the QR code, then confirm the fingerprint in the prompt that appears in the TUI.
Press `c` to copy the link, or `esc` to cancel it.

From a shell:

```sh
vibeke gateway pair
```

For limited access, use `vibeke gateway pair --scope approve` or `vibeke gateway pair --scope view`.
Each invitation is temporary and can be used once.

## Keep the connection available

After setup, the server starts the gateway whenever the server starts.
It restarts the gateway after a crash, with a retry limit. The gateway stops when the server stops.
The TUI status bar shows gateway state after setup.

The host must remain awake and online. Closing the terminal client does not stop the server or its panes.

## Manage devices

In the TUI, press `prefix` then `alt+d` to list your paired devices on the **Devices** tab of **Connections**.
Select one and press `x`, then `y`, to revoke it. Press `n` to pair a new one.
The 📱 count in the status bar shows how many devices are connected.

From a shell:

```sh
vibeke gateway devices
vibeke gateway revoke <id>
```

A revoked device cannot reconnect. Create a new invitation to pair it again.

To stop remote access and disable autostart:

```sh
vibeke gateway off
```

Use `vibeke gateway on` to enable it again.

## Install on your phone

On iOS or iPadOS, open the app in Safari and add it to the Home Screen.
Open that installed app before pairing, so you pair the app you will use.
For supported Web Push notifications, use iOS or iPadOS 16.4 or later.

Select **Settings → Alerts → Turn on** and allow notifications.
Configure notifications separately on each device.

In **Settings → Alerts**, choose when the app notifies you:

- **When an agent needs you**
- **When an agent finishes**
- **Cache going cold**: a warning before an idle agent loses its prompt cache. See [Prompt cache](#prompt-cache).

Use **Do not disturb** to pause alerts for 30 minutes, 1 hour or 4 hours.
**Notification detail** sets how much a push shows on the lock screen.
When you answer a request on another device or in the terminal, browsers that support it remove the matching notification.

## Update the app

When a new version is ready, the app shows **New version. Tap to update.** Tap **Update** to reload.
If you have unsent text, an upload in progress or an open sheet, the app waits until you finish.

## Use the app without a connection

If the host is offline when you start the app, it shows the last saved state. Each saved screen says when it was captured, for example "as of 10:42".
Reconnect to answer requests or send text.

## Share into an agent

On Android, and in the installed app, share text, links or files from another app to **Vibeke**.
The **Shared with Vibeke** screen shows what you shared.
Choose **Send to an agent**, or choose **Start a new agent with this**.
The app adds the content to the agent's draft. Review it and send it yourself.
Files go only to a running agent.

## Find your way around

- **Long-press a row** in the workspace menu to **Pin**, **Rename** or **Close pane**. Tap **Close pane** twice to confirm.
- **Search all agents** is a mode of the command palette. It searches terminals, sessions and past sessions on all your hosts. Type at least two characters.
- **Goals** lists goals planned on the host. Open a goal to read its plan and steps. Use **Approve plan** to start it, or **Cancel goal** to refuse it. A device needs full access to approve a plan.

## Start an agent

Use **New agent** to start an agent on a host.

- Choose a **Folder** from your favorite and recent folders, or choose **New folder…** to pick one on the host.
- Turn on **New worktree** to work on its own branch in a separate folder. Enter a **Branch name** and choose **Start from**. The workspace must be a Git repository.
- Tap **Again** to start another run with the same settings as an earlier one.

A view-only device cannot start agents. A device with limited access cannot use new folders or worktrees.
The host must run a version that supports this.

## Work with an agent

Open a workspace to read the conversation, or switch to its terminal.

### Drafts

The app keeps an unsent message for each pane. It is still there when you leave the pane or close the app.
If the app cannot save a draft, it warns you. Keep the app open until you send or copy the text.

### Read the conversation

- **Find in conversation** searches the messages. It loads older messages as needed.
- Use the previous and next message buttons to jump between messages you sent.
- **Jump to latest** returns to the end.
- Links and file paths in the output are tappable. A link opens in the browser. A file path opens in the file viewer.
- **Copy output** copies the output of the agent.
- **Zen** hides the controls and shows only the agent. Tap **Exit Zen** to leave. In **Settings**, **Zen in landscape** enters Zen when you turn the device sideways.

### Preview files

The file viewer previews Markdown, images, SVG and JSON. Use **Source** and **Preview** to switch views.
Large JSON shows only the first values. Images that are too large to preview show a notice.
Images in the conversation appear inline. Tap one to open it.

### Prompt cache

An agent keeps your conversation in a short-lived cache on its provider. A message sent while the cache is warm is faster and costs less.
The **Prompt cache** chip shows how long is left. Tap it for details.
Set how long each agent keeps its cache in **Settings → Prompt cache**.

### Type, speak and send keys

- Tap **Voice** to dictate. The text appears at the cursor, and you can edit it before you send it.
  The first time, choose **Use browser recognizer** or **Transcribe on host**. The browser recognizer may send audio to Apple or Google. Host transcription needs setup on the host.
- Tap **Keys** to open the keys board for terminal keys such as arrows, **esc** and **F1-F12**. Hold an arrow key to repeat it.
- Dangerous keys need a second tap. The button shows **Tap again to send**.
- **Left-hand mode** moves the keys, Send and Attach to the left side.

Use **Edit keys** to arrange the board:

- Choose a layout under **Layouts**: **Default**, **Claude Code** or **Vim**.
- Add a key with a label and up to four steps. The app sends the steps in order.
- Move a key earlier or later, change its width, or remove it.
- Use **Show code** and **Copy code** to share your layout. Paste a code and choose **Import** to use a layout from someone else.

### Assistant features

These features need the host assistant. Set it up and allow it for the workspace on the host. See the [configuration reference](reference/config.md).
Before the app sends anything, it shows the model, the size and the cost. Choose **Confirm and send** to continue.

- **While you were away** shows a card for each agent with finished turns, waiting requests and changed files. Tap **Summarize** for a written summary, **Open** to go to the agent or **Dismiss** to hide the card.
- **Suggest replies** offers short replies. Tap one to put it in the message box. The app sends nothing until you send it.

### Sandbox requests

An agent in a sandbox can ask to push a branch or copy a file out of the sandbox. The app shows a card with the request and the agent that asked.
Choose **Allow once** to permit that single action.

### Watch an agent's browser

Choose **Watch live** to see the pages that agent browsers have open on the host.
Tap the picture to click. Use **Type**, **Address** and **Keys** to control the page.
Choose **Take over** to control the page yourself, and **Release** to give it back. A device needs full access to take over.

## Troubleshooting

- Run `vibeke gateway status` to check the gateway and relay connection.
- Run `vibeke gateway logs -f` to follow its log.
- Check that the host is awake and the Vibeke server is running.
- Create a new invitation if the previous link expired or the device was revoked.

The browser stores pairing keys for each origin. Changing domains creates a separate device.
The app origin is trusted with keys and decrypted content. Check it before opening a pairing link.

For your own relay or app hosting, see [self-hosting](self-hosting.md).
