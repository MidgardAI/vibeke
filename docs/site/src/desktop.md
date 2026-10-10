# Desktop app

The desktop app gives you native notifications, a menu-bar agent list, and separate pane windows.
Choose the download for your operating system and processor.

{{#include ../../desktop-downloads.md}}

See [installation](install.md#desktop-app) for installation steps, signing details, and host requirements.

## Connect to this computer

Local connections work on supported macOS and Linux hosts.

1. [Install the host CLI](install.md#terminal-cli).
2. Start `vibeke` in your project and keep the session running.
3. Open the desktop app.
4. Select **Connect to this Mac** or **Connect to this computer**.
5. If prompted, select **Start gateway** and retry the connection.
6. Select **Open Vibeke** after the connection succeeds.

The app discovers the installed CLI and pairs over a private local socket.
If the CLI is not found, use **Choose…** to select the `vibeke` executable.

## Connect to a remote host

On the remote host, follow the [pairing instructions](mobile.md#pair-your-device).
Paste the resulting pairing link into the desktop app. Confirm the device fingerprint on the host.
The connection uses the public relay. The remote host needs no inbound port.

## Menu bar

The Vibeke icon stays in the menu bar (the system tray on Windows and Linux) while the app runs.
On macOS, the icon gets a dot when an agent needs you. A number next to it shows how many agents need you.
An agent needs you when it waits for an approval or an answer, or when it stops with an error.
An agent that finished its work also counts until you open the list.

Click the icon to open the list. Open requests come first. You can answer them directly.
Below them, the list shows every agent on every connected host, with its state and how long it has been in that state.
Select an agent to open its pane in the main window.
Set a keyboard shortcut for the list in the app settings.

## Notifications and updates

Configure alerts in the app settings. Allow notifications when your operating system asks.
The host and gateway must remain running to receive live requests.

Use **Check for Updates…** in the application menu or **Settings → About**. When a new
release is available, a button above the sidebar footer opens its details and release notes.
Choose **Download update**, then **Restart and update** when ready. The app restores your
last view and encrypted unsent conversation drafts, then reconnects to your hosts.

Background checks run every six hours and can be turned off in the update settings.
Downloading and restarting require an explicit action. Terminal composer text stays in memory
and is not restored after restarting.

In-app installation supports Windows EXE, Linux AppImage, and properly signed and notarized
Mac packages. Mac builds without publisher signing and Linux DEB installations show
**Download installer** instead. Install that package using the usual OS installation steps.
Users of v0.1.0 must manually install an update-enabled release once.

Desktop updates are separate from host CLI updates. Update each host from its terminal
interface or with `vibeke update`.
For host connection problems, run `vibeke gateway status` and `vibeke doctor` on the host.
