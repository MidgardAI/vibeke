# Desktop app

The desktop app gives you native notifications, menu-bar approvals, and separate pane windows.
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

## Notifications and updates

Configure alerts in the app settings. Allow notifications when your operating system asks.
The host and gateway must remain running to receive live requests.

To update the desktop app, download the new release and install it.
For host connection problems, run `vibeke gateway status` and `vibeke doctor` on the host.
